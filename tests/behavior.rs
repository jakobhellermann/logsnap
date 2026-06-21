//! Behavior tests for logsnap, driven through the library against an in-memory
//! filesystem (`MemFs`) — no disk, no subprocess. Each scenario captures the
//! stdout/stderr a sequence of commands produces and pins it with an `insta` inline
//! snapshot. Regenerate the inline snapshots after an intentional change with:
//!
//!     cargo insta review    # (or `cargo insta test --accept`)
//!
//! The stdout/stderr split is the core contract (content vs. headers/warnings), so
//! every snapshot shows both sections separately.

use logsnap::*;

/// Run a `diff` and render both streams for snapshotting.
fn diff_str(state: &State, fs: &dyn Fs, names: &[&str], prefix: bool) -> String {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    let mut err = Vec::new();
    diff(state, fs, &names, prefix, &mut out, &mut err).unwrap();
    render(&out, &err)
}

fn commit_str(state: &mut State, fs: &dyn Fs, names: &[&str]) -> String {
    commit_named(state, fs, names, None)
}

fn commit_named(state: &mut State, fs: &dyn Fs, names: &[&str], name: Option<&str>) -> String {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let mut err = Vec::new();
    commit(state, fs, &names, name.map(|s| s.to_string()), &mut err).unwrap();
    render(&[], &err)
}

fn list_str(state: &State) -> String {
    let mut err = Vec::new();
    list(state, "<session>", &mut err);
    render(&[], &err)
}

fn diff_in_str(state: &State, fs: &dyn Fs, at: &str, names: &[&str]) -> String {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    let mut err = Vec::new();
    diff_in(state, fs, at, &names, false, &mut out, &mut err).unwrap();
    render(&out, &err)
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
fn open_hides_existing_then_diff_shows_new() {
    let mut fs = MemFs::new();
    fs.put("player.log", "old 1\nold 2\n");
    let state = open_at_eof(&fs, &["player.log"]);

    // Nothing new yet: the two pre-existing lines must not show.
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    --- stderr ---
    === player.log: 0 new lines ===
    ");

    // After appending, only the new lines show — on stdout; the header on stderr.
    fs.append("player.log", "INFO spawn ok\nERROR null ref\n");
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    INFO spawn ok
    ERROR null ref
    --- stderr ---
    === player.log: 2 new lines ===
    ");
}

#[test]
fn diff_is_read_only_and_repeatable() {
    let mut fs = MemFs::new();
    fs.put("a.log", "");
    let state = open_at_eof(&fs, &["a.log"]);
    fs.append("a.log", "one\ntwo\n");

    let first = diff_str(&state, &fs, &[], false);
    let second = diff_str(&state, &fs, &[], false);
    assert_eq!(first, second, "diff must be idempotent");
    assert_eq!(state.files[0].cursor, 0, "diff must not move the cursor");
}

#[test]
fn partial_line_is_not_committed() {
    let mut fs = MemFs::new();
    fs.put("a.log", "");
    let mut state = open_at_eof(&fs, &["a.log"]);

    // A complete line plus a trailing partial (log mid-write).
    fs.append("a.log", "complete line\npartial no newline");
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    complete line
    --- stderr ---
    === a.log: 1 new line, +18b partial ===
    ");

    // Advancing commits only the complete line; the partial stays pending.
    commit_str(&mut state, &fs, &[]);
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    --- stderr ---
    === a.log: 0 new lines, +18b partial ===
    ");

    // Once the newline arrives, the whole line surfaces.
    fs.append("a.log", " now finished\n");
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    partial no newline now finished
    --- stderr ---
    === a.log: 1 new line ===
    ");
}

#[test]
fn commit_then_diff_is_empty_and_undo_restores() {
    let mut fs = MemFs::new();
    fs.put("a.log", "");
    let mut state = open_at_eof(&fs, &["a.log"]);
    fs.append("a.log", "l1\nl2\nl3\n");

    insta::assert_snapshot!(commit_str(&mut state, &fs, &[]), @"
    --- stdout ---
    --- stderr ---
    committed #1:
      a.log: 3 lines  [0 → 9]
    ");

    // Immediately after commit: nothing new.
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    --- stderr ---
    === a.log: 0 new lines ===
    ");

    // Undo brings the cursor (and the lines) back.
    let mut err = Vec::new();
    undo(&mut state, &mut err);
    assert_eq!(state.files[0].cursor, 0);
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    l1
    l2
    l3
    --- stderr ---
    === a.log: 3 new lines ===
    ");
}

#[test]
fn rotation_is_detected() {
    let mut fs = MemFs::new();
    fs.put("Player.log", "run1 a\nrun1 b\n");
    let mut state = open_at_eof(&fs, &["Player.log"]);
    fs.append("Player.log", "run1 c\n");
    commit_str(&mut state, &fs, &["Player.log"]);

    // Game restart: same path, brand-new inode + fresh content.
    fs.rotate("Player.log", "run2 fresh 1\nrun2 fresh 2\n");

    // The new file is read from the start, with a loud warning on stderr.
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    run2 fresh 1
    run2 fresh 2
    --- stderr ---
    === Player.log: 2 new lines ===
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
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    x
    --- stderr ---
    === app.log: 1 new line ===
        ⚠ TRUNCATED (shrank) — reading from start
    ");
}

#[test]
fn prefix_mode_tags_each_line() {
    let mut fs = MemFs::new();
    fs.put("a.log", "");
    let state = open_at_eof(&fs, &["a.log"]);
    fs.append("a.log", "first\nsecond\n");
    insta::assert_snapshot!(diff_str(&state, &fs, &["a.log"], true), @"
    --- stdout ---
    a.log: first
    a.log: second
    --- stderr ---
    === a.log: 2 new lines ===
    ");
}

#[test]
fn status_shows_line_positions() {
    let mut fs = MemFs::new();
    fs.put("player.log", "boot 1\nboot 2\n");
    fs.put("modlog.txt", "m1\n");
    let mut state = open_at_eof(&fs, &["player.log", "modlog.txt"]);
    fs.append("player.log", "new a\nnew b\nnew c\n");
    commit_str(&mut state, &fs, &["modlog.txt"]); // no-op, modlog has nothing new

    insta::assert_snapshot!(status_str(&state, &fs), @"
    --- stdout ---
    --- stderr ---
    session: <session>  (0 checkpoints)
      player.log  line 2/5   3 new
      modlog.txt  line 1/1   up to date
    ");
}

#[test]
fn file_absent_at_open_then_appears() {
    let mut fs = MemFs::new();
    // Open a log that doesn't exist yet (e.g. before the game writes it).
    let paths = ["late.log".to_string()];
    let mut err = Vec::new();
    let state = open(&fs, &paths, false, &mut err);

    // While absent: no content, a "not present" note.
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    --- stderr ---
    === late.log: 0 new lines ===
        not present
    ");

    // Once it appears, its lines show with no warning (it's a first sighting).
    fs.put("late.log", "first\nsecond\n");
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    first
    second
    --- stderr ---
    === late.log: 2 new lines ===
    ");
}

#[test]
fn file_disappears_after_open() {
    let mut fs = MemFs::new();
    fs.put("p.log", "");
    let state = open_at_eof(&fs, &["p.log"]);
    fs.append("p.log", "a\nb\n");

    // The log is deleted out from under us — reported, not silently empty.
    fs.remove("p.log");
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    --- stderr ---
    === p.log: 0 new lines ===
        DISAPPEARED since last seen
    ");
}

#[test]
fn clear_empties_session_but_keeps_files() {
    let mut fs = MemFs::new();
    fs.put("p.log", "");
    let mut state = open_at_eof(&fs, &["p.log"]);
    fs.append("p.log", "a\nb\n");
    commit_named(&mut state, &fs, &[], Some("cp"));
    fs.append("p.log", "c\n"); // pending again, plus one checkpoint

    let mut err = Vec::new();
    clear(&mut state, &fs, &mut err);

    // File still watched, history gone, cursor re-based to EOF (nothing pending).
    assert_eq!(state.files.len(), 1);
    assert!(state.history.is_empty());
    insta::assert_snapshot!(diff_str(&state, &fs, &[], false), @"
    --- stdout ---
    --- stderr ---
    === p.log: 0 new lines ===
    ");
}

#[test]
fn named_commit_appears_in_list() {
    let mut fs = MemFs::new();
    fs.put("p.log", "");
    let mut state = open_at_eof(&fs, &["p.log"]);

    fs.append("p.log", "l1\nl2\n");
    commit_named(&mut state, &fs, &[], Some("gameload"));
    fs.append("p.log", "l3\n");
    commit_str(&mut state, &fs, &[]); // anonymous checkpoint #2

    insta::assert_snapshot!(list_str(&state), @"
    --- stdout ---
    --- stderr ---
    history: <session>  (2 checkpoints)
      #1   gameload       p.log: 2 lines
      #2   -              p.log: 1 line
    ");
}

#[test]
fn recall_re_reads_a_committed_slice() {
    let mut fs = MemFs::new();
    fs.put("p.log", "");
    let mut state = open_at_eof(&fs, &["p.log"]);
    fs.append("p.log", "alpha\nbeta\n");
    commit_str(&mut state, &fs, &[]); // checkpoint #1

    // The log keeps growing; recalling #1 still shows its original slice.
    fs.append("p.log", "gamma\n");
    insta::assert_snapshot!(diff_in_str(&state, &fs, "1", &[]), @"
    --- stdout ---
    alpha
    beta
    --- stderr ---
    === p.log @ #1: 2 lines ===
    ");
}

#[test]
fn recall_is_unavailable_after_rotation() {
    let mut fs = MemFs::new();
    fs.put("Player.log", "");
    let mut state = open_at_eof(&fs, &["Player.log"]);
    fs.append("Player.log", "run1 a\nrun1 b\n");
    commit_named(&mut state, &fs, &[], Some("run1")); // #1

    // Game restart rotates the file (new inode) — the old byte range is gone, and
    // since no content was stored, the checkpoint reports it as unavailable.
    fs.rotate("Player.log", "run2 fresh\n");
    insta::assert_snapshot!(diff_in_str(&state, &fs, "run1", &[]), @r#"
    --- stdout ---
    --- stderr ---
    === Player.log @ #1 "run1": unavailable (file rotated/truncated since commit) ===
    "#);
}
