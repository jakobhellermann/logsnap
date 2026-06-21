//! A runnable walkthrough of logsnap against an in-memory log — no disk, no game.
//!
//!     cargo run --example demo
//!
//! Tweak the `fs.append(...)` / `fs.rotate(...)` calls below to see how `diff`,
//! `commit`, `undo` and rotation/truncation detection react to different log input.
//!
//! Each step taps the command's stdout and stderr into one timeline and replays it
//! interleaved, in write order — exactly as it would appear on a terminal where both
//! streams share a tty. Log *content* (stdout) is shown bright; headers and warnings
//! (stderr) are dimmed, so you can see the split that makes `logsnap diff | grep …`
//! safe while still reading the two streams in their natural order.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

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
        |_, err| {
            commit(
                &mut state,
                &fs,
                &OsClock::new(),
                &[],
                Some("spawn".into()),
                err,
            )
            .unwrap()
        },
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

    // Game restart: Player.log is rotated away (renamed, keeps its inode) and a fresh
    // one is created — so the warning can name where the old content went.
    fs.rename("Player.log", "Player-prev.log");
    fs.put("Player.log", "=== new run ===\nINFO booting\n");
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
        list(&state, &fs, "<demo>", err)
    });
    step("status — line positions and what's unseen", |_, err| {
        status(&state, &fs, "<demo>", err)
    });
}

// ---- harness: tap both streams into one timeline, replay it interleaved ----

#[derive(Clone, Copy, PartialEq)]
enum Stream {
    Out,
    Err,
}

/// Shared, write-order log of `(stream, bytes)` chunks across both sinks.
type Timeline = Rc<RefCell<Vec<(Stream, Vec<u8>)>>>;

/// A `Write` that records every write to a shared timeline, tagged with its stream,
/// so stdout and stderr can later be replayed in the exact order they were written.
struct Tap {
    stream: Stream,
    timeline: Timeline,
}

impl Write for Tap {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.timeline.borrow_mut().push((self.stream, buf.to_vec()));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn step_open(fs: &dyn Fs, paths: &[&str]) -> State {
    let owned: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
    step("open — start watching, cursors at EOF", |_out, err| {
        open(fs, &owned, false, err)
    })
}

fn step<T>(label: &str, run: impl FnOnce(&mut dyn Write, &mut dyn Write) -> T) -> T {
    let timeline = Rc::new(RefCell::new(Vec::new()));
    let mut out = Tap {
        stream: Stream::Out,
        timeline: timeline.clone(),
    };
    let mut err = Tap {
        stream: Stream::Err,
        timeline: timeline.clone(),
    };
    let result = run(&mut out, &mut err);
    print_step(label, &timeline.borrow());
    result
}

/// Replay the timeline line by line, in write order, coloring by stream. Lines never
/// mix streams (the library writes whole lines to one sink at a time), so each line's
/// stream is set by its first byte.
fn print_step(label: &str, timeline: &[(Stream, Vec<u8>)]) {
    println!("\n\x1b[1;33m# {label}\x1b[0m"); // step label: bold yellow
    let mut line = Vec::new();
    let mut stream = Stream::Out;
    for (s, bytes) in timeline {
        for &b in bytes {
            if line.is_empty() {
                stream = *s;
            }
            if b == b'\n' {
                emit(stream, &line);
                line.clear();
            } else {
                line.push(b);
            }
        }
    }
    if !line.is_empty() {
        emit(stream, &line);
    }
}

fn emit(stream: Stream, line: &[u8]) {
    let text = String::from_utf8_lossy(line);
    match stream {
        Stream::Out => println!("  {text}"), // content: bright (default)
        // The `=== file: … ===` headers get their own color; other stderr (notes,
        // warnings, commit summaries) stays dimmed.
        Stream::Err if line.starts_with(b"===") => println!("  \x1b[36m{text}\x1b[0m"), // cyan
        Stream::Err => println!("  \x1b[2m{text}\x1b[0m"),                              // dim
    }
}
