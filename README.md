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
logsnap show                         # exactly the lines that just appeared, in every file
logsnap show | grep -i null          # grep them, as often as you like — show never advances
logsnap advance                      # only now move the cursor past them
```

## Install

```
cargo install --path .
```

## Commands

| command | what it does |
| --- | --- |
| `logsnap open [--from-start] <file>...` | start a session; cursors sit at end-of-file (so only *future* lines show). `--from-start` puts them at 0. |
| `logsnap show [--prefix] [file]...` | print the new lines since the cursor. **Read-only and repeatable** — never moves the cursor. No files named = all files. `--prefix` prepends the short filename to each line (attribution when showing several files). |
| `logsnap advance [file]...` | commit: move the cursor past the new lines, reporting how many. Snapshots the prior cursors first so it can be undone. |
| `logsnap undo` | revert the last `advance`. |
| `logsnap status` (alias `view`) | per file: cursor position, file size, and how many unseen lines are pending. Your "did I forget to look at one?" dashboard. |

## The two design rules that matter

**1. Content → stdout, everything else → stderr.**
Headers, counts and warnings go to stderr; only raw log content goes to stdout. So:

```
logsnap show | grep -i error
```

filters *only* the log content, while the per-file headers and any
identity-change warning still print to your terminal — a grep that matches nothing
can never hide a warning. (Do **not** add `2>/dev/null` — that throws away exactly
those warnings.)

**2. `show` never writes; `advance` is the only thing that commits.**
You can `show | grep …` a hundred times against the same block. The cursor moves
only when you explicitly `advance`, which tells you how many lines it's committing
(`advancing past 3 line(s) [22 → 62]`) — so you see what you're discarding before
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
committed by `advance` (`+23b partial kept`), so you never advance past half a line.

## State

A single session lives in `$XDG_STATE_HOME/logsnap/state.json` (by default
`~/.local/state/logsnap/state.json`). Override the location with `$LOGSNAP_STATE`.

## Example: a Hollow Knight mod debug loop

```
cd ~/my-mod
logsnap open \
  ~/.config/unity3d/Team\ Cherry/Hollow\ Knight/Player.log \
  ~/.config/unity3d/Team\ Cherry/Hollow\ Knight/ModLog.txt

# trigger a spawn in-game, then:
logsnap show                 # everything the spawn produced, both logs
logsnap show | grep -iE 'error|null|exception'
logsnap view                 # "ModLog.txt: 5 new" reminds you not to skip it
logsnap advance              # understood — move on to the next iteration
```
