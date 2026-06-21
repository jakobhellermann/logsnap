//! The commands, parameterized over the [`Fs`] backend and `Write` sinks. By
//! convention log *content* goes to `out` and headers/warnings go to `err`, so a
//! `show | grep` pipe filters only content.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::clock::Clock;
use crate::cursor::{Event, line_stats, region, resolve};
use crate::fs::Fs;
use crate::state::{Commit, CommitEntry, FileState, State};

pub fn short(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

/// Does a session/commit-entry `path` match a user-supplied `name` (exact path,
/// file name, or path suffix)?
fn path_matches(path: &str, name: &str) -> bool {
    path == name || short(path) == name || path.ends_with(name)
}

/// Plural suffix for regular nouns: "" for one, "s" otherwise (incl. zero).
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Write log content to `out`, optionally prefixing each line with `tag`.
fn write_lines(out: &mut dyn Write, bytes: &[u8], prefix: bool, tag: &str) {
    if prefix {
        for line in bytes.split_inclusive(|&b| b == b'\n') {
            let _ = write!(out, "{tag}: ");
            let _ = out.write_all(line);
        }
    } else {
        let _ = out.write_all(bytes);
    }
}

/// Find the file that used to live at `path` — i.e. a sibling whose `(dev, ino)`
/// matches the identity we last recorded — after it was rotated away under a new name.
fn find_rotated(fs: &dyn Fs, path: &str, dev: u64, ino: u64) -> Option<String> {
    fs.siblings(path)
        .into_iter()
        .find(|p| matches!(fs.stat(p), Some(s) if s.dev == dev && s.ino == ino))
}

/// A per-file note for `err`, given the file's *previously recorded* identity `old`
/// (so a rotation can name the file the old inode was renamed to).
fn event_note(fs: &dyn Fs, path: &str, old: (u64, u64), ev: Event) -> Option<String> {
    match ev {
        Event::Ok | Event::Appeared => None,
        Event::Missing => Some("not present".into()),
        Event::Disappeared => Some("DISAPPEARED since last seen".into()),
        Event::Truncated => Some("⚠ TRUNCATED (shrank) — reading from start".into()),
        Event::Rotated => {
            let base = "⚠ IDENTITY CHANGED (rotated/replaced) — reading the new file from start";
            Some(match find_rotated(fs, path, old.0, old.1) {
                Some(prev) => format!("{base}; previous content is now in {}", short(&prev)),
                None => format!("{base}; the previous content is no longer at this path"),
            })
        }
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
        let idx = state.files.iter().position(|f| path_matches(&f.path, n));
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

/// Empty the session in place: re-baseline every cursor to end-of-file (so nothing
/// is pending) and drop the commit history. Keeps the watched files — does NOT end
/// the session.
pub fn clear(state: &mut State, fs: &dyn Fs, err: &mut dyn Write) {
    for f in &mut state.files {
        match fs.stat(&f.path) {
            Some(s) => {
                f.cursor = s.size;
                f.dev = s.dev;
                f.ino = s.ino;
                let _ = writeln!(err, "  {}  (cursor at EOF)", short(&f.path));
            }
            None => {
                let _ = writeln!(err, "  {}  (not present)", short(&f.path));
            }
        }
    }
    let dropped = state.history.len();
    state.history.clear();
    state.next_id = 0;
    let _ = writeln!(
        err,
        "session emptied: {} checkpoint{} dropped",
        dropped,
        plural(dropped)
    );
}

/// Print the new (uncommitted) lines since each cursor. Read-only: never mutates
/// `state`. This is the everyday view — the "diff" between the committed point and now.
pub fn diff(
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

        let mut hdr = format!(
            "=== {}: {} new line{}",
            short(&f.path),
            reg.lines,
            plural(reg.lines)
        );
        if reg.partial > 0 {
            hdr.push_str(&format!(", +{}b partial", reg.partial));
        }
        hdr.push_str(" ===");
        let _ = writeln!(err, "{hdr}");
        if let Some(note) = event_note(fs, &f.path, (f.dev, f.ino), ev) {
            let _ = writeln!(err, "    {note}");
        }

        write_lines(out, &reg.bytes, prefix, short(&f.path));
    }
    let _ = out.flush();
    Ok(())
}

/// Move each cursor past the new lines, recording a checkpoint in the history (so
/// `undo` can revert it and `diff --in` can re-read it). Reports how many lines.
pub fn commit(
    state: &mut State,
    fs: &dyn Fs,
    clock: &dyn Clock,
    names: &[String],
    message: Option<String>,
    err: &mut dyn Write,
) -> Result<(), String> {
    let sel = select(state, names)?;

    let mut entries = Vec::new();
    for i in sel {
        let path = state.files[i].path.clone();
        let st = fs.stat(&path);
        let (from, ev) = resolve(&state.files[i], &st);
        if let Some(note) = event_note(fs, &path, (state.files[i].dev, state.files[i].ino), ev) {
            let _ = writeln!(err, "  {}: {note}", short(&path));
        }
        if ev.absent() {
            continue;
        }
        let reg = region(&fs.read(&path).unwrap_or_default(), from);

        let f = &state.files[i];
        let (prev_cursor, prev_dev, prev_ino) = (f.cursor, f.dev, f.ino);
        let (dev, ino) = st.map(|s| (s.dev, s.ino)).unwrap_or((f.dev, f.ino));
        let moved = reg.lines > 0 || ev != Event::Ok || reg.end != prev_cursor;

        let f = &mut state.files[i];
        f.cursor = reg.end;
        f.dev = dev;
        f.ino = ino;

        if moved {
            entries.push(CommitEntry {
                path,
                from,
                to: reg.end,
                dev,
                ino,
                lines: reg.lines,
                prev_cursor,
                prev_dev,
                prev_ino,
            });
        }
    }

    if entries.is_empty() {
        let _ = writeln!(err, "nothing to commit");
        return Ok(());
    }

    let id = state.next_id.max(1);
    state.next_id = id + 1;
    let label = message
        .as_deref()
        .map(|m| format!(" \"{m}\""))
        .unwrap_or_default();
    let _ = writeln!(err, "committed #{id}{label}:");
    for e in &entries {
        let _ = writeln!(
            err,
            "  {}: {} line{}  [{} → {}]",
            short(&e.path),
            e.lines,
            plural(e.lines),
            e.from,
            e.to
        );
    }
    state.history.push(Commit {
        id,
        message,
        created_at: Some(clock.now_hms()),
        entries,
    });
    const MAX_HISTORY: usize = 200;
    let len = state.history.len();
    if len > MAX_HISTORY {
        state.history.drain(0..len - MAX_HISTORY);
    }
    Ok(())
}

/// True if any complete line in `bytes` contains `needle` (empty needle never matches).
fn any_line_contains(bytes: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }
    bytes
        .split(|&b| b == b'\n')
        .any(|line| line.windows(needle.len()).any(|w| w == needle))
}

/// How long the blocking commits keep polling before giving up on a log that never
/// goes quiet (e.g. per-frame logging that changes every poll).
pub const SETTLE_DEADLINE: Duration = Duration::from_secs(5);

/// A duration rounded to the nearest whole millisecond (for human-facing timings).
fn millis_round(d: Duration) -> u64 {
    (d.as_secs_f64() * 1000.0).round() as u64
}

/// Block until a committable (complete) line containing `needle` appears in a targeted
/// file, polling every `interval` (re-reading a file only when its `(dev, ino, size)`
/// changed since the last scan — stat-gated). `Ok` once matched; `Err` if `at_most`
/// elapses first. A pure wait — never mutates `state`.
#[allow(clippy::too_many_arguments)]
fn await_line(
    state: &State,
    fs: &dyn Fs,
    clock: &dyn Clock,
    sel: &[usize],
    needle: &str,
    at_most: Duration,
    interval: Duration,
    err: &mut dyn Write,
) -> Result<(), String> {
    let needle_b = needle.as_bytes();
    // The (dev, ino, size) we last scanned per selected file; `None` = absent / not yet
    // scanned. Unchanged key → no new bytes → skip the read.
    let mut last: Vec<Option<(u64, u64, u64)>> = vec![None; sel.len()];

    loop {
        for (k, &i) in sel.iter().enumerate() {
            let st = fs.stat(&state.files[i].path);
            let key = st.map(|s| (s.dev, s.ino, s.size));
            if key == last[k] {
                continue;
            }
            last[k] = key;
            let (from, ev) = resolve(&state.files[i], &st);
            if ev.absent() {
                continue;
            }
            let path = &state.files[i].path;
            let reg = region(&fs.read(path).unwrap_or_default(), from);
            if any_line_contains(&reg.bytes, needle_b) {
                let _ = writeln!(err, "\"{needle}\" appeared in {}", short(path));
                return Ok(());
            }
        }
        if clock.elapsed() >= at_most {
            return Err(format!(
                "timed out after {at_most:?} waiting for \"{needle}\"; nothing committed"
            ));
        }
        clock.sleep(interval);
    }
}

/// Block until no targeted file's `(dev, ino, size)` has changed for `settle` — any
/// observed change restarts the quiet window. `Ok` once settled; `Err` if it never
/// settles within [`SETTLE_DEADLINE`] (measured from entry, so it composes after a
/// preceding wait). A pure wait — never mutates `state`.
fn await_quiet(
    state: &State,
    fs: &dyn Fs,
    clock: &dyn Clock,
    sel: &[usize],
    settle: Duration,
    interval: Duration,
) -> Result<Duration, String> {
    let key_of = |i: usize| {
        fs.stat(&state.files[i].path)
            .map(|s| (s.dev, s.ino, s.size))
    };
    // The (dev, ino, size) per selected file at the last poll. `None` = absent (a stable
    // state that can itself settle).
    let mut last: Vec<Option<(u64, u64, u64)>> = sel.iter().map(|&i| key_of(i)).collect();
    let start = clock.elapsed();
    let mut quiet_since = start;

    loop {
        if clock.elapsed() - quiet_since >= settle {
            return Ok(clock.elapsed() - start);
        }
        if clock.elapsed() - start >= SETTLE_DEADLINE {
            return Err(format!(
                "did not settle within {SETTLE_DEADLINE:?} (log kept changing); nothing committed"
            ));
        }
        clock.sleep(interval);

        let mut changed = false;
        for (k, &i) in sel.iter().enumerate() {
            let key = key_of(i);
            if key != last[k] {
                last[k] = key;
                changed = true;
            }
        }
        if changed {
            quiet_since = clock.elapsed();
        }
    }
}

/// Wait for a committable line containing `needle` (≤ `at_most`), then [`commit`]. On
/// timeout the cursor is left untouched and this returns `Err` (abort, don't commit).
#[allow(clippy::too_many_arguments)]
pub fn commit_wait(
    state: &mut State,
    fs: &dyn Fs,
    clock: &dyn Clock,
    names: &[String],
    needle: &str,
    at_most: Duration,
    interval: Duration,
    message: Option<String>,
    err: &mut dyn Write,
) -> Result<(), String> {
    let sel = select(state, names)?;
    if needle.is_empty() {
        return Err("--wait-for needs a non-empty substring".into());
    }
    let _ = writeln!(err, "waiting for \"{needle}\" (≤ {at_most:?})…");
    await_line(state, fs, clock, &sel, needle, at_most, interval, err)?;
    commit(state, fs, clock, names, message, err)
}

/// Wait until the targeted files have been quiet for `settle`, then [`commit`]. If the
/// log never settles within [`SETTLE_DEADLINE`], returns `Err` (didn't settle, don't commit).
#[allow(clippy::too_many_arguments)]
pub fn commit_settle(
    state: &mut State,
    fs: &dyn Fs,
    clock: &dyn Clock,
    names: &[String],
    settle: Duration,
    interval: Duration,
    message: Option<String>,
    err: &mut dyn Write,
) -> Result<(), String> {
    let sel = select(state, names)?;
    let _ = writeln!(
        err,
        "settling (quiet for {settle:?}, give up after {SETTLE_DEADLINE:?})…"
    );
    let waited = await_quiet(state, fs, clock, &sel, settle, interval)?;
    let _ = writeln!(err, "settled after {}ms", millis_round(waited));
    commit(state, fs, clock, names, message, err)
}

/// Combined gate: first wait for a line containing `needle` (≤ `at_most`), then wait
/// until the log is quiet for `settle`, then [`commit`]. Either wait failing aborts
/// without committing.
#[allow(clippy::too_many_arguments)]
pub fn commit_wait_settle(
    state: &mut State,
    fs: &dyn Fs,
    clock: &dyn Clock,
    names: &[String],
    needle: &str,
    at_most: Duration,
    settle: Duration,
    interval: Duration,
    message: Option<String>,
    err: &mut dyn Write,
) -> Result<(), String> {
    let sel = select(state, names)?;
    if needle.is_empty() {
        return Err("--wait-for needs a non-empty substring".into());
    }
    let _ = writeln!(err, "waiting for \"{needle}\" (≤ {at_most:?})…");
    await_line(state, fs, clock, &sel, needle, at_most, interval, err)?;
    let _ = writeln!(
        err,
        "settling (quiet for {settle:?}, give up after {SETTLE_DEADLINE:?})…"
    );
    let waited = await_quiet(state, fs, clock, &sel, settle, interval)?;
    let _ = writeln!(err, "settled after {}ms", millis_round(waited));
    commit(state, fs, clock, names, message, err)
}

/// Fold the pending (uncommitted) lines into the most recent checkpoint, extending its
/// committed range to the current cursor — like `git commit --amend` / `jj squash` into
/// the parent. `undo` still reverts the whole (now-larger) checkpoint in one step, since
/// each entry keeps the `prev_*` it had before the original commit.
pub fn squash(
    state: &mut State,
    fs: &dyn Fs,
    names: &[String],
    err: &mut dyn Write,
) -> Result<(), String> {
    if state.history.is_empty() {
        return Err("no checkpoint to squash into — commit first".into());
    }
    let sel = select(state, names)?;

    // What each file folds into the last checkpoint. `contiguous` means the pending
    // region sits right after the existing entry on the same (dev, ino), so it can
    // simply extend it; otherwise the identity broke and the range is replaced.
    struct Fold {
        path: String,
        from: u64,
        to: u64,
        dev: u64,
        ino: u64,
        lines: usize,
        prev_cursor: u64,
        prev_dev: u64,
        prev_ino: u64,
        contiguous: bool,
    }

    let mut folds = Vec::new();
    for i in sel {
        let path = state.files[i].path.clone();
        let st = fs.stat(&path);
        let (from, ev) = resolve(&state.files[i], &st);
        if let Some(note) = event_note(fs, &path, (state.files[i].dev, state.files[i].ino), ev) {
            let _ = writeln!(err, "  {}: {note}", short(&path));
        }
        if ev.absent() {
            continue;
        }
        let reg = region(&fs.read(&path).unwrap_or_default(), from);

        let f = &state.files[i];
        let (prev_cursor, prev_dev, prev_ino) = (f.cursor, f.dev, f.ino);
        let (dev, ino) = st.map(|s| (s.dev, s.ino)).unwrap_or((f.dev, f.ino));
        let moved = reg.lines > 0 || ev != Event::Ok || reg.end != prev_cursor;

        let f = &mut state.files[i];
        f.cursor = reg.end;
        f.dev = dev;
        f.ino = ino;

        if moved {
            folds.push(Fold {
                path,
                from,
                to: reg.end,
                dev,
                ino,
                lines: reg.lines,
                prev_cursor,
                prev_dev,
                prev_ino,
                contiguous: ev == Event::Ok,
            });
        }
    }

    if folds.is_empty() {
        let _ = writeln!(err, "nothing to squash");
        return Ok(());
    }

    let last = state.history.last_mut().expect("checked non-empty above");
    for fold in &folds {
        match last.entries.iter_mut().find(|e| e.path == fold.path) {
            // Pure extension of a still-matching range.
            Some(e)
                if fold.contiguous
                    && e.to == fold.from
                    && e.dev == fold.dev
                    && e.ino == fold.ino =>
            {
                e.to = fold.to;
                e.lines += fold.lines;
            }
            // Identity broke (rotation/truncation): the old slice is gone, so replace
            // the range with the new file's. Keep `prev_*` so undo reverts as before.
            Some(e) => {
                e.from = fold.from;
                e.to = fold.to;
                e.dev = fold.dev;
                e.ino = fold.ino;
                e.lines = fold.lines;
            }
            // File wasn't part of this checkpoint yet — add it.
            None => last.entries.push(CommitEntry {
                path: fold.path.clone(),
                from: fold.from,
                to: fold.to,
                dev: fold.dev,
                ino: fold.ino,
                lines: fold.lines,
                prev_cursor: fold.prev_cursor,
                prev_dev: fold.prev_dev,
                prev_ino: fold.prev_ino,
            }),
        }
    }

    let label = last
        .message
        .as_deref()
        .map(|m| format!(" \"{m}\""))
        .unwrap_or_default();
    let _ = writeln!(err, "squashed into #{}{label}:", last.id);
    for fold in &folds {
        let _ = writeln!(
            err,
            "  {}: +{} line{}  [→ {}]",
            short(&fold.path),
            fold.lines,
            plural(fold.lines),
            fold.to
        );
    }
    Ok(())
}

/// Revert the most recent [`commit`], restoring each file's prior cursor.
pub fn undo(state: &mut State, err: &mut dyn Write) {
    match state.history.pop() {
        None => {
            let _ = writeln!(err, "nothing to undo");
        }
        Some(c) => {
            for e in &c.entries {
                if let Some(f) = state.files.iter_mut().find(|f| f.path == e.path) {
                    let was = f.cursor;
                    f.cursor = e.prev_cursor;
                    f.dev = e.prev_dev;
                    f.ino = e.prev_ino;
                    if was != e.prev_cursor {
                        let _ = writeln!(
                            err,
                            "  {}: cursor {} → {}",
                            short(&f.path),
                            was,
                            e.prev_cursor
                        );
                    }
                }
            }
            state.next_id = c.id; // reuse the id for the next commit
            let label = c
                .message
                .as_deref()
                .map(|m| format!(" \"{m}\""))
                .unwrap_or_default();
            let left = state.history.len();
            let _ = writeln!(
                err,
                "undone #{}{label}; {} checkpoint{} left",
                c.id,
                left,
                plural(left)
            );
        }
    }
}

/// Find a checkpoint by numeric id or by message.
fn find_commit<'a>(state: &'a State, at: &str) -> Option<&'a Commit> {
    if let Ok(id) = at.parse::<u32>() {
        state.history.iter().find(|c| c.id == id)
    } else {
        state
            .history
            .iter()
            .find(|c| c.message.as_deref() == Some(at))
    }
}

/// List the commit history (id, message, per-file line counts), then a one-line
/// footer summarizing what is still uncommitted (the files with pending lines).
pub fn list(state: &State, fs: &dyn Fs, session_label: &str, err: &mut dyn Write) {
    let n = state.history.len();
    let _ = writeln!(
        err,
        "history: {session_label}  ({n} checkpoint{})",
        plural(n)
    );
    if state.history.is_empty() {
        let _ = writeln!(err, "  (none yet — `commit` to create one)");
    } else {
        for c in &state.history {
            let when = c.created_at.as_deref().unwrap_or("--:--:--");
            let msg = c.message.as_deref().unwrap_or("-");
            let files = c
                .entries
                .iter()
                .map(|e| format!("{}: {} line{}", short(&e.path), e.lines, plural(e.lines)))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(err, "  #{:<3} {when}  {msg:<14} {files}", c.id);
        }
    }

    // Footer: only the files that actually have something pending.
    let pending: Vec<String> = state
        .files
        .iter()
        .filter_map(|f| {
            let st = fs.stat(&f.path);
            let (from, ev) = resolve(f, &st);
            if ev.absent() {
                return None;
            }
            let reg = region(&fs.read(&f.path).unwrap_or_default(), from);
            match (reg.lines, reg.partial) {
                (0, 0) => None,
                (0, p) => Some(format!("{} +{}b partial", short(&f.path), p)),
                (l, 0) => Some(format!("{} {} new", short(&f.path), l)),
                (l, p) => Some(format!("{} {} new +{}b partial", short(&f.path), l, p)),
            }
        })
        .collect();
    let _ = if pending.is_empty() {
        writeln!(err, "uncommitted: none")
    } else {
        writeln!(err, "uncommitted: {}", pending.join(", "))
    };
}

/// Re-show the lines recorded in a past checkpoint (`diff --in <ref>`), by re-reading
/// each file's committed byte range — but only while the file's identity still matches
/// (no content is stored, so a rotated/truncated file's old slice is unavailable).
pub fn diff_in(
    state: &State,
    fs: &dyn Fs,
    at: &str,
    names: &[String],
    prefix: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), String> {
    let commit = find_commit(state, at).ok_or_else(|| format!("no checkpoint: {at}"))?;
    let label = commit
        .message
        .as_deref()
        .map(|m| format!(" \"{m}\""))
        .unwrap_or_default();

    for e in &commit.entries {
        if !names.is_empty() && !names.iter().any(|n| path_matches(&e.path, n)) {
            continue;
        }
        let st = fs.stat(&e.path);
        let available = matches!(st, Some(s) if s.dev == e.dev && s.ino == e.ino && s.size >= e.to);
        if !available {
            let _ = writeln!(
                err,
                "=== {} @ #{}{label}: unavailable (file rotated/truncated since commit) ===",
                short(&e.path),
                commit.id
            );
            continue;
        }
        let bytes = fs.read(&e.path).unwrap_or_default();
        let slice = &bytes[e.from as usize..e.to as usize];
        let _ = writeln!(
            err,
            "=== {} @ #{}{label}: {} line{} ===",
            short(&e.path),
            commit.id,
            e.lines,
            plural(e.lines)
        );
        write_lines(out, slice, prefix, short(&e.path));
    }
    let _ = out.flush();
    Ok(())
}

/// Per-file dashboard: cursor as a line position, and how many lines are unseen.
pub fn status(state: &State, fs: &dyn Fs, session_label: &str, err: &mut dyn Write) {
    let n = state.history.len();
    let _ = writeln!(
        err,
        "session: {session_label}  ({n} checkpoint{})",
        plural(n)
    );
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
        if let Some(note) = event_note(fs, &f.path, (f.dev, f.ino), ev) {
            line.push_str(&format!("   {note}"));
        }
        let _ = writeln!(err, "{line}");
    }
}
