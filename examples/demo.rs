//! A runnable walkthrough of logsnap against an in-memory log — no disk, no game.
//!
//!     cargo run --example demo
//!
//! Tweak the `fs.append(...)` / `fs.rotate(...)` calls below to see how `diff`,
//! `commit`, `undo` and rotation/truncation detection react to different log input.
//! Each step prints the command's stdout and stderr separately, so you can see the
//! split that makes `logsnap diff | grep …` safe (content on stdout, headers and
//! warnings on stderr).

use logsnap::*;

fn main() {
    let mut fs = MemFs::new();

    // --- the "input" you can tweak ----------------------------------------
    // Two logs that already have some history before we start watching.
    fs.put("Player.log", "earlier boot line\n");
    fs.put("ModLog.txt", "[API] mod loaded\n");

    // open: cursors sit at end-of-file, so existing history is ignored.
    let mut state = step_open(&fs, &["Player.log", "ModLog.txt"]);

    // The game does something — new lines land in both logs.
    fs.append(
        "Player.log",
        "INFO spawn ok\nNullReferenceException: boom\nINFO frame\n",
    );
    fs.append("ModLog.txt", "[HornetPlayer] spawned hero\n");
    step("diff — exactly what just appeared", |out, err| {
        diff(&state, &fs, &[], false, out, err).unwrap()
    });

    step(
        "commit -m spawn — record a checkpoint with a message",
        |_, err| commit(&mut state, &fs, &[], Some("spawn".into()), err).unwrap(),
    );
    step("diff again — nothing new", |out, err| {
        diff(&state, &fs, &[], false, out, err).unwrap()
    });

    // A trailing partial line (the log is mid-write): shown, but not committed.
    fs.append("Player.log", "INFO half-written line with no newline yet");
    step(
        "diff — partial line is shown but stays pending",
        |out, err| diff(&state, &fs, &[], false, out, err).unwrap(),
    );

    // Recall the named checkpoint — re-reads its committed slice while identity holds.
    step(
        "diff --in spawn — recall the checkpoint's lines",
        |out, err| diff_in(&state, &fs, "spawn", &[], false, out, err).unwrap(),
    );

    // Game restart: Player.log is rotated away and recreated (new inode).
    fs.rotate("Player.log", "=== new run ===\nINFO booting\n");
    step(
        "diff — rotation detected, new file read from start",
        |out, err| diff(&state, &fs, &[], false, out, err).unwrap(),
    );
    // The pre-restart checkpoint's bytes are gone now (offsets only, no stored content).
    step(
        "diff --in spawn — now unavailable for the rotated file",
        |out, err| diff_in(&state, &fs, "spawn", &[], false, out, err).unwrap(),
    );

    step("list — the commit history", |_, err| {
        list(&state, "<demo>", err)
    });
    step("status — line positions and what's unseen", |_, err| {
        status(&state, &fs, "<demo>", err)
    });
}

// ---- tiny harness: run a step, print its two streams labelled -------------

fn step_open(fs: &dyn Fs, paths: &[&str]) -> State {
    let owned: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
    let mut err = Vec::new();
    let state = open(fs, &owned, false, &mut err);
    print_step("open — start watching, cursors at EOF", &[], &err);
    state
}

fn step(label: &str, run: impl FnOnce(&mut Vec<u8>, &mut Vec<u8>)) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    run(&mut out, &mut err);
    print_step(label, &out, &err);
}

fn print_step(label: &str, out: &[u8], err: &[u8]) {
    println!("\n\x1b[1m# {label}\x1b[0m");
    if !out.is_empty() {
        println!("  \x1b[2m── stdout (log content) ──\x1b[0m");
        for line in String::from_utf8_lossy(out).lines() {
            println!("  {line}");
        }
    }
    if !err.is_empty() {
        println!("  \x1b[2m── stderr (headers/warnings) ──\x1b[0m");
        for line in String::from_utf8_lossy(err).lines() {
            println!("  \x1b[2m{line}\x1b[0m");
        }
    }
}
