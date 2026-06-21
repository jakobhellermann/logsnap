//! logsnap — stateful, cursor-based log snapshotting for iterative debugging.
//!
//! The workflow it formalizes: note where a log file is *now*, trigger some action
//! (a spawn, a request, a rebuild), then look at exactly the lines that action
//! produced — grep them as often as you like — and only when satisfied advance the
//! cursor past them. Multi-file, and aware of file-identity changes (a log getting
//! rotated/replaced, e.g. Unity moving Player.log -> Player-prev.log).
//!
//! Design invariants:
//! - `show` and `status`/`view` are PURELY read-only; they never touch the state.
//!   So `logsnap show | grep Error` that matches nothing loses nothing.
//! - Headers, meta and warnings go to STDERR; raw log content goes to STDOUT. A
//!   pipe (`| grep`) filters only the content; an identity-change warning is never
//!   swallowed by a grep.
//! - `advance` reports exactly how many lines it commits ("discards" from `show`),
//!   and snapshots the prior cursors first so `undo` can revert it.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default)]
struct FileState {
    path: String,
    #[serde(default)]
    dev: u64,
    #[serde(default)]
    ino: u64,
    /// Byte offset of the committed read position (start of the next unseen line).
    #[serde(default)]
    cursor: u64,
}

/// One entry per file, captured before an `advance`, so `undo` can restore it.
#[derive(Serialize, Deserialize, Clone)]
struct Snap {
    cursor: u64,
    dev: u64,
    ino: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// Open files, in the order they were passed to `open`.
    files: Vec<FileState>,
    /// Stack of pre-advance snapshots (newest last). Each is one Snap per file.
    #[serde(default)]
    undo: Vec<Vec<Snap>>,
}

// ---- file-identity / read resolution -------------------------------------

struct Stat {
    dev: u64,
    ino: u64,
    size: u64,
}

fn stat(path: &str) -> Option<Stat> {
    let m = fs::metadata(path).ok()?;
    Some(Stat {
        dev: m.dev(),
        ino: m.ino(),
        size: m.size(),
    })
}

#[derive(PartialEq)]
enum Event {
    Ok,
    Missing,     // file not present (and we have no prior identity)
    Disappeared, // we had it, now it's gone
    Appeared,    // we had no identity, now it's there
    Rotated,     // identity changed: replaced/rotated
    Truncated,   // same identity but shrank below the cursor
}

/// Decide where to read from, given the current on-disk state. Pure — does not
/// mutate `fs`. Returns the effective byte offset to read from and what happened.
fn resolve(fsr: &FileState, st: &Option<Stat>) -> (u64, Event) {
    match st {
        None => {
            if fsr.ino == 0 {
                (0, Event::Missing)
            } else {
                (fsr.cursor, Event::Disappeared)
            }
        }
        Some(s) => {
            if fsr.ino == 0 {
                // Never had an identity (opened while absent): adopt, read from cursor.
                (fsr.cursor.min(s.size), Event::Appeared)
            } else if s.dev != fsr.dev || s.ino != fsr.ino {
                (0, Event::Rotated)
            } else if s.size < fsr.cursor {
                (0, Event::Truncated)
            } else {
                (fsr.cursor, Event::Ok)
            }
        }
    }
}

struct Region {
    /// Complete lines (everything up to and including the last '\n'), raw bytes.
    bytes: Vec<u8>,
    /// Absolute byte offset just past the last complete line.
    end: u64,
    /// Number of complete lines read.
    lines: usize,
    /// Bytes of a trailing partial (no newline yet) line, kept uncommitted.
    partial: usize,
}

fn read_region(path: &str, from: u64) -> io::Result<Region> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    match buf.iter().rposition(|&b| b == b'\n') {
        Some(idx) => {
            let complete = buf[..=idx].to_vec();
            let lines = complete.iter().filter(|&&b| b == b'\n').count();
            let partial = buf.len() - (idx + 1);
            Ok(Region {
                bytes: complete,
                end: from + idx as u64 + 1,
                lines,
                partial,
            })
        }
        None => Ok(Region {
            bytes: Vec::new(),
            end: from,
            lines: 0,
            partial: buf.len(),
        }),
    }
}

fn count_lines(path: &str) -> io::Result<usize> {
    let mut f = File::open(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf.iter().filter(|&&b| b == b'\n').count())
}

// ---- state location / persistence ----------------------------------------

const STATE_NAME: &str = "state.json";
const STATE_DIR: &str = ".logsnap";

/// Find the state file path. `LOGSNAP_STATE` overrides; otherwise walk up from the
/// cwd looking for an existing `.logsnap/state.json`. Returns None if none exists.
fn find_state() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LOGSNAP_STATE") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let cand = dir.join(STATE_DIR).join(STATE_NAME);
        if cand.exists() {
            return Some(cand);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Where a fresh `open` should create the state file.
fn new_state_path() -> PathBuf {
    if let Ok(p) = std::env::var("LOGSNAP_STATE") {
        return PathBuf::from(p);
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(STATE_DIR)
        .join(STATE_NAME)
}

fn load_state() -> Result<(State, PathBuf), String> {
    let path = find_state().ok_or_else(|| {
        "no logsnap session here (or above). Start one with: logsnap open <files...>".to_string()
    })?;
    let data = fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let state: State =
        serde_json::from_str(&data).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    Ok((state, path))
}

fn save_state(state: &State, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    fs::write(path, data).map_err(|e| format!("writing {}: {e}", path.display()))
}

fn abspath(s: &str) -> String {
    let p = PathBuf::from(s);
    let abs = if p.is_absolute() {
        p
    } else {
        std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p)
    };
    // Normalize without requiring existence (canonicalize would fail on a not-yet-
    // created Player.log). Just collapse . and .. lexically.
    let mut out = PathBuf::new();
    for comp in abs.components() {
        use std::path::Component::*;
        match comp {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other),
        }
    }
    out.to_string_lossy().into_owned()
}

fn short(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

// ---- commands -------------------------------------------------------------

fn cmd_open(args: &[String]) -> Result<(), String> {
    let mut from_start = false;
    let mut files = Vec::new();
    for a in args {
        match a.as_str() {
            "--from-start" | "-s" => from_start = true,
            _ => files.push(abspath(a)),
        }
    }
    if files.is_empty() {
        return Err("open: need at least one file".into());
    }

    let mut state = State::default();
    let mut err = io::stderr();
    for path in files {
        let st = stat(&path);
        let (dev, ino, cursor, note) = match &st {
            Some(s) => {
                let cursor = if from_start { 0 } else { s.size };
                let n = count_lines(&path).unwrap_or(0);
                let pending = if from_start { n } else { 0 };
                (
                    s.dev,
                    s.ino,
                    cursor,
                    format!(
                        "{n} lines{}",
                        if from_start {
                            format!(", {pending} pending")
                        } else {
                            String::new()
                        }
                    ),
                )
            }
            None => (0, 0, 0, "not present yet".to_string()),
        };
        let _ = writeln!(err, "  {}  ({note})", short(&path));
        state.files.push(FileState {
            path,
            dev,
            ino,
            cursor,
        });
    }
    let path = new_state_path();
    save_state(&state, &path)?;
    let _ = writeln!(err, "session: {}", path.display());
    Ok(())
}

/// Resolve which files a command targets (named subset, or all if none named).
fn select(state: &State, names: &[String]) -> Result<Vec<usize>, String> {
    if names.is_empty() {
        return Ok((0..state.files.len()).collect());
    }
    let mut out = Vec::new();
    for n in names {
        let want = abspath(n);
        let idx = state.files.iter().position(|f| {
            f.path == want || short(&f.path) == n.as_str() || f.path.ends_with(n.as_str())
        });
        match idx {
            Some(i) => out.push(i),
            None => return Err(format!("not in session: {n}")),
        }
    }
    Ok(out)
}

fn event_note(ev: &Event) -> Option<String> {
    match ev {
        Event::Ok | Event::Appeared => None,
        Event::Missing => Some("not present".into()),
        Event::Disappeared => Some("DISAPPEARED since last seen".into()),
        Event::Rotated => Some(
            "⚠ IDENTITY CHANGED (rotated/replaced) — reading new file from start; \
             prior content may be in a rotated file (e.g. *-prev.log)"
                .into(),
        ),
        Event::Truncated => Some("⚠ TRUNCATED (shrank) — reading from start".into()),
    }
}

fn cmd_show(args: &[String]) -> Result<(), String> {
    let mut prefix = false;
    let mut names = Vec::new();
    for a in args {
        match a.as_str() {
            "--prefix" | "-p" => prefix = true,
            _ => names.push(a.clone()),
        }
    }
    let (state, _) = load_state()?;
    let sel = select(&state, &names)?;

    let mut out = io::stdout().lock();
    let mut err = io::stderr();
    for i in sel {
        let f = &state.files[i];
        let st = stat(&f.path);
        let (from, ev) = resolve(f, &st);
        let region = if matches!(ev, Event::Missing | Event::Disappeared) {
            Region {
                bytes: Vec::new(),
                end: from,
                lines: 0,
                partial: 0,
            }
        } else {
            read_region(&f.path, from).map_err(|e| format!("reading {}: {e}", f.path))?
        };

        let mut hdr = format!("=== {}: {} new line(s)", short(&f.path), region.lines);
        if region.partial > 0 {
            hdr.push_str(&format!(", +{}b partial", region.partial));
        }
        hdr.push_str(" ===");
        let _ = writeln!(err, "{hdr}");
        if let Some(note) = event_note(&ev) {
            let _ = writeln!(err, "    {note}");
        }

        if prefix {
            let tag = short(&f.path);
            for line in region.bytes.split_inclusive(|&b| b == b'\n') {
                let _ = write!(out, "{tag}: ");
                let _ = out.write_all(line);
            }
        } else {
            let _ = out.write_all(&region.bytes);
        }
    }
    let _ = out.flush();
    Ok(())
}

fn cmd_advance(args: &[String]) -> Result<(), String> {
    let (mut state, path) = load_state()?;
    let sel = select(&state, args)?;

    // Snapshot ALL files (not just the selected) so undo restores the whole session.
    let snapshot: Vec<Snap> = state
        .files
        .iter()
        .map(|f| Snap {
            cursor: f.cursor,
            dev: f.dev,
            ino: f.ino,
        })
        .collect();

    let mut err = io::stderr();
    let mut any = false;
    for i in sel {
        let f = &state.files[i];
        let st = stat(&f.path);
        let (from, ev) = resolve(f, &st);
        if let Some(note) = event_note(&ev) {
            let _ = writeln!(err, "  {}: {note}", short(&f.path));
        }
        if matches!(ev, Event::Missing | Event::Disappeared) {
            continue;
        }
        let region = read_region(&f.path, from).map_err(|e| format!("reading {}: {e}", f.path))?;

        let f = &mut state.files[i];
        let before = f.cursor;
        f.cursor = region.end;
        if let Some(s) = &st {
            f.dev = s.dev;
            f.ino = s.ino;
        }
        let moved = ev != Event::Ok || region.end != before;
        if region.lines > 0 || moved {
            any = true;
        }
        let extra = if region.partial > 0 {
            format!(" (+{}b partial kept)", region.partial)
        } else {
            String::new()
        };
        let _ = writeln!(
            err,
            "  {}: advancing past {} line(s){extra}  [{} → {}]",
            short(&f.path),
            region.lines,
            before,
            f.cursor
        );
    }

    if any {
        state.undo.push(snapshot);
        const MAX_UNDO: usize = 100;
        let len = state.undo.len();
        if len > MAX_UNDO {
            state.undo.drain(0..len - MAX_UNDO);
        }
    } else {
        let _ = writeln!(err, "nothing to advance");
    }
    save_state(&state, &path)
}

fn cmd_undo(_args: &[String]) -> Result<(), String> {
    let (mut state, path) = load_state()?;
    let mut err = io::stderr();
    match state.undo.pop() {
        None => {
            let _ = writeln!(err, "nothing to undo");
        }
        Some(snap) => {
            for (f, s) in state.files.iter_mut().zip(snap.iter()) {
                let was = f.cursor;
                f.cursor = s.cursor;
                f.dev = s.dev;
                f.ino = s.ino;
                if was != s.cursor {
                    let _ = writeln!(err, "  {}: cursor {} → {}", short(&f.path), was, s.cursor);
                }
            }
            let _ = writeln!(
                err,
                "undone (1 advance); {} left on undo stack",
                state.undo.len()
            );
        }
    }
    save_state(&state, &path)
}

fn cmd_status(_args: &[String]) -> Result<(), String> {
    let (state, spath) = load_state()?;
    let mut err = io::stderr();
    let _ = writeln!(
        err,
        "session: {}  ({} undo)",
        spath.display(),
        state.undo.len()
    );
    // Column the names.
    let w = state
        .files
        .iter()
        .map(|f| short(&f.path).len())
        .max()
        .unwrap_or(0);
    for f in &state.files {
        let st = stat(&f.path);
        let (from, ev) = resolve(f, &st);
        let pending = if matches!(ev, Event::Missing | Event::Disappeared) {
            None
        } else {
            read_region(&f.path, from)
                .ok()
                .map(|r| (r.lines, r.partial))
        };
        let size = st.as_ref().map(|s| s.size).unwrap_or(0);
        let mut line = format!(
            "  {:<w$}  cursor {}/{}",
            short(&f.path),
            f.cursor,
            size,
            w = w
        );
        match pending {
            Some((0, 0)) => line.push_str("   up to date"),
            Some((n, p)) => {
                line.push_str(&format!("   {n} new"));
                if p > 0 {
                    line.push_str(&format!(" (+{p}b partial)"));
                }
            }
            None => {}
        }
        if let Some(note) = event_note(&ev) {
            line.push_str(&format!("   {note}"));
        }
        let _ = writeln!(err, "{line}");
    }
    Ok(())
}

fn usage() {
    eprintln!(
        "logsnap — cursor-based log snapshotting (multi-file, rotation-aware)

USAGE:
  logsnap open [--from-start] <file>...   start a session; cursors at EOF (or 0)
  logsnap show [--prefix] [file]...       print new lines since cursor (READ-ONLY, repeatable)
  logsnap advance [file]...               commit past the new lines (snapshots for undo)
  logsnap undo                            revert the last advance
  logsnap status | view                   per-file cursor + how many lines are unseen

Content goes to stdout; headers/warnings to stderr — so `logsnap show | grep X`
filters only content and never swallows an identity-change warning.

State: .logsnap/state.json in the cwd (walks up to find it); override with $LOGSNAP_STATE."
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            usage();
            return ExitCode::FAILURE;
        }
    };
    let res = match cmd {
        "open" => cmd_open(rest),
        "show" => cmd_show(rest),
        "advance" | "adv" => cmd_advance(rest),
        "undo" => cmd_undo(rest),
        "status" | "view" | "st" => cmd_status(rest),
        "help" | "-h" | "--help" => {
            usage();
            return ExitCode::SUCCESS;
        }
        other => Err(format!("unknown command: {other}\n(try: logsnap help)")),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("logsnap: {e}");
            ExitCode::FAILURE
        }
    }
}
