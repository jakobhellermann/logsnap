//! The commands, parameterized over the [`Fs`] backend and `Write` sinks. By
//! convention log *content* goes to `out` and headers/warnings go to `err`, so a
//! `show | grep` pipe filters only content.

use std::io::Write;
use std::path::Path;

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

/// Move each cursor past the new lines, recording a checkpoint in the history (so
/// `undo` can revert it and `show --at` can re-read it). Reports how many lines.
pub fn commit(
    state: &mut State,
    fs: &dyn Fs,
    names: &[String],
    name: Option<String>,
    err: &mut dyn Write,
) -> Result<(), String> {
    let sel = select(state, names)?;

    let mut entries = Vec::new();
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
    let label = name
        .as_deref()
        .map(|n| format!(" \"{n}\""))
        .unwrap_or_default();
    let _ = writeln!(err, "committed #{id}{label}:");
    for e in &entries {
        let _ = writeln!(
            err,
            "  {}: {} line(s)  [{} → {}]",
            short(&e.path),
            e.lines,
            e.from,
            e.to
        );
    }
    state.history.push(Commit { id, name, entries });
    const MAX_HISTORY: usize = 200;
    let len = state.history.len();
    if len > MAX_HISTORY {
        state.history.drain(0..len - MAX_HISTORY);
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
                .name
                .as_deref()
                .map(|n| format!(" \"{n}\""))
                .unwrap_or_default();
            let _ = writeln!(
                err,
                "undone #{}{label}; {} checkpoint(s) left",
                c.id,
                state.history.len()
            );
        }
    }
}

/// Find a checkpoint by numeric id or by name.
fn find_commit<'a>(state: &'a State, at: &str) -> Option<&'a Commit> {
    if let Ok(id) = at.parse::<u32>() {
        state.history.iter().find(|c| c.id == id)
    } else {
        state.history.iter().find(|c| c.name.as_deref() == Some(at))
    }
}

/// List the commit history (id, name, per-file line counts).
pub fn list(state: &State, session_label: &str, err: &mut dyn Write) {
    let _ = writeln!(
        err,
        "history: {session_label}  ({} checkpoint(s))",
        state.history.len()
    );
    if state.history.is_empty() {
        let _ = writeln!(err, "  (none yet — `commit` to create one)");
        return;
    }
    for c in &state.history {
        let name = c.name.as_deref().unwrap_or("-");
        let files = c
            .entries
            .iter()
            .map(|e| format!("{}: {} lines", short(&e.path), e.lines))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(err, "  #{:<3} {:<14} {}", c.id, name, files);
    }
}

/// Re-show the lines a past checkpoint recorded, by re-reading each file's committed
/// byte range — but only while the file's identity still matches (no content is
/// stored, so a rotated/truncated file's old slice is reported as unavailable).
pub fn show_at(
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
        .name
        .as_deref()
        .map(|n| format!(" \"{n}\""))
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
            "=== {} @ #{}{label}: {} line(s) ===",
            short(&e.path),
            commit.id,
            e.lines
        );
        if prefix {
            let tag = short(&e.path);
            for line in slice.split_inclusive(|&b| b == b'\n') {
                let _ = write!(out, "{tag}: ");
                let _ = out.write_all(line);
            }
        } else {
            let _ = out.write_all(slice);
        }
    }
    let _ = out.flush();
    Ok(())
}

/// Per-file dashboard: cursor as a line position, and how many lines are unseen.
pub fn status(state: &State, fs: &dyn Fs, session_label: &str, err: &mut dyn Write) {
    let _ = writeln!(
        err,
        "session: {session_label}  ({} checkpoint(s))",
        state.history.len()
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
        if let Some(note) = event_note(ev) {
            line.push_str(&format!("   {note}"));
        }
        let _ = writeln!(err, "{line}");
    }
}
