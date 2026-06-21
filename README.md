# logsnap

Stateful, cursor-based log snapshotting for iterative debugging.

## Why

When you debug by repeatedly triggering an action and reading its log output, plain
`grep`/`tail` have three failure modes:

- Missing lines: `grep ERROR` only shows what you thought to look for, you might miss unexpected lines
- It's can be unclear which lines are from an old run and which are new
- Forgetting a file: Two logs to watch, you only checked one.

`logsnap` fixes this by encoding a resilient workflow into a small stateful CLI:

```sh
logsnap open Player.log ModLog.txt
# ... trigger the thing ...
logsnap diff                         # exactly the lines that just appeared, in every file
logsnap commit                       # mark current lines as read
# try something else
logsnap diff | grep NullReferenceException
logsnap diff | grep Exception
```

You can also attach names to snapshots, and review them later:
```sh
logsnap commit -m ok
logsnap commit
logsnap commit -m broken

logsnap diff --in ok
logsnap diff --in broken
```

## Install

```sh
cargo install --path .
```

## Commands

| command | what it does |
| --- | --- |
| `logsnap open [--from-start] <file>...` | start a session; cursors sit at end-of-file (so only *future* lines show). `--from-start` puts them at 0. |
| `logsnap diff [--prefix] [--in <ref>] [file]...` | print the new lines since the cursor. **Read-only and repeatable** — never moves the cursor. No files named = all files. `--prefix` prepends the short filename to each line. `--in <ref>` instead re-shows the lines a past checkpoint recorded. |
| `logsnap commit [-m <message>] [file]...` | move the cursor past the new lines (recording a checkpoint in the history), reporting how many. `-m`/`--message` labels the checkpoint for `list` / `diff --in`. |
| `logsnap commit --wait-for <substr> --at-most <dur> [file]...` | block until a complete line containing `<substr>` appears in a watched file, then commit. Polls (default every 20ms, `--interval` to change). On timeout (`2s`, `500ms`, `1m`, …) it leaves the session untouched and exits non-zero — handy for `trigger-thing && logsnap commit --wait-for Ready --at-most 5s`. |
| `logsnap commit --settle <dur> [file]...` | block until the watched files have been quiet (no new bytes) for `<dur>`, then commit — for "let the action finish, then snapshot". Reports how long it actually waited. Gives up after a fixed 5s if the log never settles (e.g. per-frame logging), exiting non-zero. Combine with `--wait-for` to wait for the trigger line first, *then* for quiet. |
| `logsnap squash [file]...` | fold the pending lines into the *most recent* checkpoint instead of opening a new one (like `git commit --amend`): its committed range extends to the current cursor, keeping the same id and message. `undo` still reverts the whole checkpoint. |
| `logsnap undo` | revert the last `commit`. |
| `logsnap list` | the commit history: each checkpoint's id, local creation time (`HH:MM:SS`), message, and per-file line counts, plus an `uncommitted:` footer naming the files with pending lines. |
| `logsnap status` | per file: cursor position and how many unseen lines are pending. |
| `logsnap clear` | empty the session in place: re-baseline cursors to EOF and drop the history. Keeps watching the same files. |

### Checkpoints & recall

Each `commit` records a checkpoint (`#1`, `#2`, … — label them with `-m gameload`).
`logsnap list` shows the history; `logsnap diff --in gameload` (or `--in 1`) re-shows the
lines that checkpoint committed.

Recall stores **only byte offsets + the file's identity**, not the log content, so it
has to re-read the file. That works as long as the file's identity still matches; once the file
has rotated or been truncated (e.g. a game restart), that checkpoint's slice is gone and
`diff --in` reports it as *unavailable*.

## File-identity awareness

`logsnap` tracks each file by `(device, inode)`:

- **Rotation / replacement** (e.g. Unity moving `Player.log` → `Player-prev.log` and
  starting a fresh `Player.log` on restart): the inode changes, so `logsnap` warns
  `⚠ IDENTITY CHANGED` and reads the new file from the start instead of silently
  showing nothing or reading at a stale offset. If the old inode is still in the same
  directory under a new name, the warning names it (found by inode, not guessed).
- **Truncation in place** (file rewritten smaller, same inode): detected via
  `size < cursor`; warns `⚠ TRUNCATED` and reads from 0.

A trailing line with no newline yet (the log is mid-write) is shown but **not**
committed by `commit` (`+23b partial kept`), so you never commit past half a line.

## State

A single session lives in `$XDG_STATE_HOME/logsnap/state.json` (by default
`~/.local/state/logsnap/state.json`). Override the location with `$LOGSNAP_STATE`.

## Example: a Hollow Knight mod debug loop

```sh
cd ~/my-mod
logsnap open \
  "~/.config/unity3d/Team Cherry/Hollow Knight/Player.log" \
  "~/.config/unity3d/Team Cherry/Hollow Knight/ModLog.txt"

logsnap commit -m "load"

# do some stuff ingame
logsnap diff                 # everything that happened in both logs
logsnap diff | grep -iE 'error|null|exception'
logsnap status               # "ModLog.txt: 5 new" reminds you not to skip it
logsnap commit               # move on to the next iteration

# wait for a specific log message, and for things to settle afterwards
logsnap commit --wait-for "[MenuMods] instantiated button" --settle 200ms --at-most 5s
```
