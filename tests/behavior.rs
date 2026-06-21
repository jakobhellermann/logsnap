//! Behavior tests for logsnap, driven through the library against an in-memory
//! filesystem (`MemFs`) — no disk, no subprocess. Each scenario captures the
//! stdout/stderr a sequence of commands produces and pins it with an `insta` inline
//! snapshot. Regenerate snapshots after an intentional change with:
//!
//!     INSTA_UPDATE=always cargo test -p logsnap
//!     # or, with the cargo-insta tool: cargo insta review
//!
//! The stdout/stderr split is the core contract (content vs. headers/warnings), so
//! every snapshot shows both sections separately.

use logsnap::*;

/// Run a `show` and render both streams for snapshotting.
fn show_str(state: &State, fs: &dyn Fs, names: &[&str], prefix: bool) -> String {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    let mut err = Vec::new();
    show(state, fs, &names, prefix, &mut out, &mut err).unwrap();
    render(&out, &err)
}

fn advance_str(state: &mut State, fs: &dyn Fs, names: &[&str]) -> String {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let mut err = Vec::new();
    advance(state, fs, &names, &mut err).unwrap();
    render(&[], &err)
}

fn status_str(state: &State, fs: &dyn Fs) -> String {
    let mut err = Vec::new();
    status(state, fs, "<session>", &mut err);
    render(&[], &err)
}

fn render(out: &[u8], err: &[u8]) -> String {
    format!(
        "--- stdout ---\n{}--- stderr ---\n{}",
        String::from_utf8_lossy(out),
        String::from_utf8_lossy(err),
    )
}

fn open_at_eof(fs: &dyn Fs, paths: &[&str]) -> State {
    let paths: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
    let mut err = Vec::new();
    open(fs, &paths, false, &mut err)
}

#[test]
fn open_hides_existing_then_show_new() {
    let mut fs = MemFs::new();
    fs.put("player.log", "old 1\nold 2\n");
    let state = open_at_eof(&fs, &["player.log"]);

    // Nothing new yet: the two pre-existing lines must not show.
    insta::assert_snapshot!(show_str(&state, &fs, &[], false), @r"
    --- stdout ---
    --- stderr ---
    === player.log: 0 new line(s) ===
    ");

    // After appending, only the new lines show — on stdout; the header on stderr.
    fs.append("player.log", "INFO spawn ok\nERROR null ref\n");
    insta::assert_snapshot!(show_str(&state, &fs, &[], false), @r"
    --- stdout ---
    INFO spawn ok
    ERROR null ref
    --- stderr ---
    === player.log: 2 new line(s) ===
    ");
}

#[test]
fn show_is_read_only_and_repeatable() {
    let mut fs = MemFs::new();
    fs.put("a.log", "");
    let state = open_at_eof(&fs, &["a.log"]);
    fs.append("a.log", "one\ntwo\n");

    let first = show_str(&state, &fs, &[], false);
    let second = show_str(&state, &fs, &[], false);
    assert_eq!(first, second, "show must be idempotent");
    assert_eq!(state.files[0].cursor, 0, "show must not move the cursor");
}

#[test]
fn partial_line_is_not_committed() {
    let mut fs = MemFs::new();
    fs.put("a.log", "");
    let mut state = open_at_eof(&fs, &["a.log"]);

    // A complete line plus a trailing partial (log mid-write).
    fs.append("a.log", "complete line\npartial no newline");
    insta::assert_snapshot!(show_str(&state, &fs, &[], false), @r"
    --- stdout ---
    complete line
    --- stderr ---
    === a.log: 1 new line(s), +18b partial ===
    ");

    // Advancing commits only the complete line; the partial stays pending.
    advance_str(&mut state, &fs, &[]);
    insta::assert_snapshot!(show_str(&state, &fs, &[], false), @r"
    --- stdout ---
    --- stderr ---
    === a.log: 0 new line(s), +18b partial ===
    ");

    // Once the newline arrives, the whole line surfaces.
    fs.append("a.log", " now finished\n");
    insta::assert_snapshot!(show_str(&state, &fs, &[], false), @r"
    --- stdout ---
    partial no newline now finished
    --- stderr ---
    === a.log: 1 new line(s) ===
    ");
}

#[test]
fn advance_then_show_is_empty_and_undo_restores() {
    let mut fs = MemFs::new();
    fs.put("a.log", "");
    let mut state = open_at_eof(&fs, &["a.log"]);
    fs.append("a.log", "l1\nl2\nl3\n");

    insta::assert_snapshot!(advance_str(&mut state, &fs, &[]), @r"
    --- stdout ---
    --- stderr ---
      a.log: advancing past 3 line(s)  [0 → 9]
    ");

    // Immediately after advance: nothing new.
    insta::assert_snapshot!(show_str(&state, &fs, &[], false), @r"
    --- stdout ---
    --- stderr ---
    === a.log: 0 new line(s) ===
    ");

    // Undo brings the cursor (and the lines) back.
    let mut err = Vec::new();
    undo(&mut state, &mut err);
    assert_eq!(state.files[0].cursor, 0);
    insta::assert_snapshot!(show_str(&state, &fs, &[], false), @r"
    --- stdout ---
    l1
    l2
    l3
    --- stderr ---
    === a.log: 3 new line(s) ===
    ");
}

#[test]
fn rotation_is_detected() {
    let mut fs = MemFs::new();
    fs.put("Player.log", "run1 a\nrun1 b\n");
    let mut state = open_at_eof(&fs, &["Player.log"]);
    fs.append("Player.log", "run1 c\n");
    advance_str(&mut state, &fs, &["Player.log"]);

    // Game restart: same path, brand-new inode + fresh content.
    fs.rotate("Player.log", "run2 fresh 1\nrun2 fresh 2\n");

    // The new file is read from the start, with a loud warning on stderr.
    insta::assert_snapshot!(show_str(&state, &fs, &[], false), @r"
    --- stdout ---
    run2 fresh 1
    run2 fresh 2
    --- stderr ---
    === Player.log: 2 new line(s) ===
        ⚠ IDENTITY CHANGED (rotated/replaced) — reading new file from start; prior content may be in a rotated file (e.g. *-prev.log)
    ");
}

#[test]
fn truncation_is_detected() {
    let mut fs = MemFs::new();
    fs.put("app.log", "a\nb\nc\nd\n");
    let state = open_at_eof(&fs, &["app.log"]); // cursor at EOF (8)

    // Rewritten in place, smaller, same inode.
    fs.put("app.log", "x\n");
    insta::assert_snapshot!(show_str(&state, &fs, &[], false), @r"
    --- stdout ---
    x
    --- stderr ---
    === app.log: 1 new line(s) ===
        ⚠ TRUNCATED (shrank) — reading from start
    ");
}

#[test]
fn prefix_mode_tags_each_line() {
    let mut fs = MemFs::new();
    fs.put("a.log", "");
    let state = open_at_eof(&fs, &["a.log"]);
    fs.append("a.log", "first\nsecond\n");
    insta::assert_snapshot!(show_str(&state, &fs, &["a.log"], true), @r"
    --- stdout ---
    a.log: first
    a.log: second
    --- stderr ---
    === a.log: 2 new line(s) ===
    ");
}

#[test]
fn status_shows_line_positions() {
    let mut fs = MemFs::new();
    fs.put("player.log", "boot 1\nboot 2\n");
    fs.put("modlog.txt", "m1\n");
    let mut state = open_at_eof(&fs, &["player.log", "modlog.txt"]);
    fs.append("player.log", "new a\nnew b\nnew c\n");
    advance_str(&mut state, &fs, &["modlog.txt"]); // no-op, modlog has nothing new

    insta::assert_snapshot!(status_str(&state, &fs), @r"
    --- stdout ---
    --- stderr ---
    session: <session>  (0 undo)
      player.log  line 2/5   3 new
      modlog.txt  line 1/1   up to date
    ");
}
