//! The commands, parameterized over the [`Fs`] backend and `Write` sinks. By
//! convention log *content* goes to `out` and headers/warnings go to `err`, so a
//! `show | grep` pipe filters only content.

use std::io::Write;
use std::path::Path;

use crate::cursor::{Event, line_stats, region, resolve};
use crate::fs::Fs;
use crate::state::{FileState, Snap, State};

pub fn short(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

fn event_note(ev: Event) -> Option<&'static str> {
    match ev {
        Event::Ok | Event::Appeared => None,
        Event::Missing => Some("not present"),
        Event::Disappeared => Some("DISAPPEARED since last seen"),
        Event::Rotated => Some(
            "⚠ IDENTITY CHANGED (rotated/replaced) — reading new file from start; \
             prior content may be in a rotated file (e.g. *-prev.log)",
        ),
        Event::Truncated => Some("⚠ TRUNCATED (shrank) — reading from start"),
    }
}

/// Which files a command targets: a named subset, or all if none named. A name
/// matches by exact path, by file name, or as a path suffix.
pub fn select(state: &State, names: &[String]) -> Result<Vec<usize>, String> {
    if names.is_empty() {
        return Ok((0..state.files.len()).collect());
    }
    let mut out = Vec::new();
    for n in names {
        let idx = state.files.iter().position(|f| {
            f.path == *n || short(&f.path) == n.as_str() || f.path.ends_with(n.as_str())
        });
        match idx {
            Some(i) => out.push(i),
            None => return Err(format!("not in session: {n}")),
        }
    }
    Ok(out)
}

/// Build a fresh session over `paths`. Cursors sit at end-of-file (only future lines
/// show) unless `from_start`. Per-file notes go to `err`.
pub fn open(fs: &dyn Fs, paths: &[String], from_start: bool, err: &mut dyn Write) -> State {
    let mut state = State::default();
    for path in paths {
        let st = fs.stat(path);
        let (dev, ino, cursor, note) = match st {
            Some(s) => {
                let bytes = fs.read(path).unwrap_or_default();
                let n = bytes.iter().filter(|&&b| b == b'\n').count();
                let cursor = if from_start { 0 } else { s.size };
                let note = if from_start {
                    format!("{n} lines, {n} pending")
                } else {
                    format!("{n} lines")
                };
                (s.dev, s.ino, cursor, note)
            }
            None => (0, 0, 0, "not present yet".to_string()),
        };
        let _ = writeln!(err, "  {}  ({note})", short(path));
        state.files.push(FileState {
            path: path.clone(),
            dev,
            ino,
            cursor,
        });
    }
    state
}

/// Print the new lines since each cursor. Read-only: never mutates `state`.
pub fn show(
    state: &State,
    fs: &dyn Fs,
    names: &[String],
    prefix: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), String> {
    for i in select(state, names)? {
        let f = &state.files[i];
        let st = fs.stat(&f.path);
        let (from, ev) = resolve(f, &st);
        let reg = if ev.absent() {
            region(&[], from)
        } else {
            region(&fs.read(&f.path).unwrap_or_default(), from)
        };

        let mut hdr = format!("=== {}: {} new line(s)", short(&f.path), reg.lines);
        if reg.partial > 0 {
            hdr.push_str(&format!(", +{}b partial", reg.partial));
        }
        hdr.push_str(" ===");
        let _ = writeln!(err, "{hdr}");
        if let Some(note) = event_note(ev) {
            let _ = writeln!(err, "    {note}");
        }

        if prefix {
            let tag = short(&f.path);
            for line in reg.bytes.split_inclusive(|&b| b == b'\n') {
                let _ = write!(out, "{tag}: ");
                let _ = out.write_all(line);
            }
        } else {
            let _ = out.write_all(&reg.bytes);
        }
    }
    let _ = out.flush();
    Ok(())
}

/// Move each cursor past the new lines, reporting how many. Snapshots all cursors
/// first so [`undo`] can revert it.
pub fn commit(
    state: &mut State,
    fs: &dyn Fs,
    names: &[String],
    err: &mut dyn Write,
) -> Result<(), String> {
    let sel = select(state, names)?;
    let snapshot: Vec<Snap> = state
        .files
        .iter()
        .map(|f| Snap {
            cursor: f.cursor,
            dev: f.dev,
            ino: f.ino,
        })
        .collect();

    let mut any = false;
    for i in sel {
        let path = state.files[i].path.clone();
        let st = fs.stat(&path);
        let (from, ev) = resolve(&state.files[i], &st);
        if let Some(note) = event_note(ev) {
            let _ = writeln!(err, "  {}: {note}", short(&path));
        }
        if ev.absent() {
            continue;
        }
        let reg = region(&fs.read(&path).unwrap_or_default(), from);

        let f = &mut state.files[i];
        let before = f.cursor;
        f.cursor = reg.end;
        if let Some(s) = st {
            f.dev = s.dev;
            f.ino = s.ino;
        }
        if reg.lines > 0 || ev != Event::Ok || reg.end != before {
            any = true;
        }
        let extra = if reg.partial > 0 {
            format!(" (+{}b partial kept)", reg.partial)
        } else {
            String::new()
        };
        let _ = writeln!(
            err,
            "  {}: committing {} line(s){extra}  [{} → {}]",
            short(&path),
            reg.lines,
            before,
            reg.end
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
        let _ = writeln!(err, "nothing to commit");
    }
    Ok(())
}

/// Revert the most recent [`commit`].
pub fn undo(state: &mut State, err: &mut dyn Write) {
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
                "undone (1 commit); {} left on undo stack",
                state.undo.len()
            );
        }
    }
}

/// Per-file dashboard: cursor as a line position, and how many lines are unseen.
pub fn status(state: &State, fs: &dyn Fs, session_label: &str, err: &mut dyn Write) {
    let _ = writeln!(err, "session: {session_label}  ({} undo)", state.undo.len());
    let w = state
        .files
        .iter()
        .map(|f| short(&f.path).len())
        .max()
        .unwrap_or(0);
    for f in &state.files {
        let st = fs.stat(&f.path);
        let (from, ev) = resolve(f, &st);
        let bytes = if ev.absent() {
            Vec::new()
        } else {
            fs.read(&f.path).unwrap_or_default()
        };
        let reg = region(&bytes, from);
        let (at_line, total) = if ev.absent() {
            (0, 0)
        } else {
            line_stats(&bytes, from)
        };
        let mut line = format!(
            "  {:<w$}  line {}/{}",
            short(&f.path),
            at_line,
            total,
            w = w
        );
        if !ev.absent() {
            if reg.lines == 0 && reg.partial == 0 {
                line.push_str("   up to date");
            } else {
                line.push_str(&format!("   {} new", reg.lines));
                if reg.partial > 0 {
                    line.push_str(&format!(" (+{}b partial)", reg.partial));
                }
            }
        }
        if let Some(note) = event_note(ev) {
            line.push_str(&format!("   {note}"));
        }
        let _ = writeln!(err, "{line}");
    }
}
