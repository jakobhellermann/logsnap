# logsnap

Stateful, cursor-based log snapshotting for iterative debugging — multi-file and
file-identity aware (it notices when a log gets rotated or truncated out from under
you).

## Why

When you debug by repeatedly triggering an action and reading its log output, plain
`grep`/`tail` have three failure modes:

- **You miss lines.** `grep ERROR` only shows what you thought to look for; the
  *unexpected* line is exactly the one you don't grep for.
- **Old vs. new is unclear.** Was that error from this run or the last one?
- **You forget a file.** Two logs to watch, you only checked one.

`logsnap` formalizes the "note the offset, do the thing, read exactly the new
lines" workflow into a small stateful CLI:

```
logsnap open Player.log ModLog.txt   # remember where each file is now
# ... trigger the thing ...
logsnap diff                         # exactly the lines that just appeared, in every file
logsnap diff | grep -i null          # grep them, as often as you like — diff never commits
logsnap commit                       # only now move the cursor past them
```

## Install

```
cargo install --path .
```

## Commands

| command | what it does |
| --- | --- |
| `logsnap open [--from-start] <file>...` | start a session; cursors sit at end-of-file (so only *future* lines show). `--from-start` puts them at 0. |
| `logsnap diff [--prefix] [--in <ref>] [file]...` | print the new lines since the cursor. **Read-only and repeatable** — never moves the cursor. No files named = all files. `--prefix` prepends the short filename to each line. `--in <ref>` instead re-shows the lines a past checkpoint recorded (see below). |
| `logsnap commit [--name <name>] [file]...` | move the cursor past the new lines (recording a checkpoint in the history), reporting how many. `--name` labels the checkpoint for `list` / `diff --in`. |
| `logsnap undo` | revert the last `commit`. |
| `logsnap list` | the commit history: each checkpoint's id, name, and per-file line counts. |
| `logsnap status` | per file: cursor position (as a line number) and how many unseen lines are pending. Your "did I forget to look at one?" dashboard. |
| `logsnap clear` | empty the session in place: re-baseline cursors to EOF (nothing pending) and drop the history. Keeps watching the same files. |

### Checkpoints & recall

Each `commit` records a checkpoint (`#1`, `#2`, … — name them with `--name gameload`).
`logsnap list` shows the history; `logsnap diff --in gameload` (or `--in 1`) re-shows the
lines that checkpoint committed.

Recall stores **only byte offsets + the file's identity**, not the log content — so it
re-reads the file. That works as long as the file's identity still matches; once the file
has rotated or been truncated (e.g. a game restart), that checkpoint's slice is gone and
`diff --in` reports it as *unavailable* for that file rather than printing stale bytes.

## The two design rules that matter

**1. Content → stdout, everything else → stderr.**
Headers, counts and warnings go to stderr; only raw log content goes to stdout. So:

```
logsnap diff | grep -i error
```

filters *only* the log content, while the per-file headers and any
identity-change warning still print to your terminal — a grep that matches nothing
can never hide a warning. (Do **not** add `2>/dev/null` — that throws away exactly
those warnings.)

**2. `diff` never writes; `commit` is the only thing that moves the cursor.**
You can `diff | grep …` a hundred times against the same block. The cursor moves
only when you explicitly `commit`, which tells you how many lines it's recording
(`committing 3 lines [22 → 62]`) — so you see what you're discarding before
it's gone. `undo` brings it back.

## File-identity awareness

`logsnap` tracks each file by `(device, inode)`, not just its name:

- **Rotation / replacement** (e.g. Unity moving `Player.log` → `Player-prev.log` and
  starting a fresh `Player.log` on restart): the inode changes, so `logsnap` warns
  `⚠ IDENTITY CHANGED` and reads the new file from the start instead of silently
  showing nothing or reading at a stale offset.
- **Truncation in place** (file rewritten smaller, same inode): detected via
  `size < cursor`; warns `⚠ TRUNCATED` and reads from 0.

A trailing line with no newline yet (the log is mid-write) is shown but **not**
committed by `commit` (`+23b partial kept`), so you never commit past half a line.

## State

A single session lives in `$XDG_STATE_HOME/logsnap/state.json` (by default
`~/.local/state/logsnap/state.json`). Override the location with `$LOGSNAP_STATE`.

## Development

The logic lives in `src/lib.rs`, decoupled from the filesystem (a `Fs` trait — real
`OsFs` vs. in-memory `MemFs`) and from stdout/stderr (commands write to caller-supplied
`Write` sinks). `src/main.rs` is a thin CLI wrapper.

- **Tests** (`tests/behavior.rs`): in-process behavior tests against `MemFs`, pinned
  with `insta` inline snapshots — no disk, no subprocess. `MemFs` models growth,
  rotation (new inode) and truncation explicitly.

  ```
  cargo test            # check
  cargo insta review    # update the inline snapshots after an intentional change
  ```

- **Demo** (`examples/demo.rs`): a runnable walkthrough you can tweak to watch the
  behavior on different log input.

  ```
  cargo run --example demo
  ```

## Example: a Hollow Knight mod debug loop

```
cd ~/my-mod
logsnap open \
  ~/.config/unity3d/Team\ Cherry/Hollow\ Knight/Player.log \
  ~/.config/unity3d/Team\ Cherry/Hollow\ Knight/ModLog.txt

# trigger a spawn in-game, then:
logsnap diff                 # everything the spawn produced, both logs
logsnap diff | grep -iE 'error|null|exception'
logsnap status               # "ModLog.txt: 5 new" reminds you not to skip it
logsnap commit               # understood — move on to the next iteration
```
