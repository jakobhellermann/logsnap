//! logsnap CLI — thin clap wrapper that wires the real filesystem ([`OsFs`]) and
//! stdout/stderr into the library command functions, plus session load/save and
//! dynamic shell completion.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::{CommandFactory, Parser, Subcommand, ValueHint};
use clap_complete::CompleteEnv;
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};

use logsnap::*;

#[derive(Parser)]
#[command(
    name = "logsnap",
    version,
    about = "cursor-based log snapshotting (multi-file, rotation-aware)",
    after_help = "Content goes to stdout; headers/warnings to stderr — so `logsnap diff | grep X` \
                  filters only content and never swallows an identity-change warning.\n\
                  State: $XDG_STATE_HOME/logsnap/state.json (~/.local/state/...); override with $LOGSNAP_STATE."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a session; cursors sit at EOF (only future lines show) unless --from-start.
    Open {
        /// Put cursors at the start of each file instead of at EOF.
        #[arg(short = 's', long = "from-start")]
        from_start: bool,
        #[arg(required = true, value_name = "FILE", value_hint = ValueHint::FilePath)]
        files: Vec<String>,
    },
    /// Print the new (uncommitted) lines since the cursor (READ-ONLY, repeatable).
    Diff {
        /// Prefix each line with the file name (attribution across files).
        #[arg(short, long)]
        prefix: bool,
        /// Follow: keep printing new lines as they arrive (like `tail -f`).
        /// Uses inotify on Linux, polling otherwise.
        #[arg(short = 'f', long)]
        follow: bool,
        /// Poll interval for --follow (polling fallback only; inotify is event-driven).
        #[arg(long, value_name = "DURATION", value_parser = parse_duration, default_value = "20ms")]
        interval: Duration,
        /// Re-show the lines recorded in a past checkpoint instead of pending lines.
        /// REF is a message, an absolute id (`1`), or `^N` from the end (`^`/`^1` = latest).
        #[arg(long = "in", value_name = "REF", add = ArgValueCompleter::new(complete_checkpoints))]
        in_ref: Option<String>,
        #[arg(value_name = "FILE", add = ArgValueCompleter::new(complete_session_files))]
        files: Vec<String>,
    },
    /// Commit past the new lines (records a checkpoint; revert with undo).
    Commit {
        /// Message for this checkpoint (its label in `list` and its `diff --in <msg>` ref).
        #[arg(short, long)]
        message: Option<String>,
        /// Block until a complete line containing this substring appears, then commit.
        #[arg(long = "wait-for", value_name = "SUBSTR", requires = "at_most")]
        wait_for: Option<String>,
        /// Give up waiting after this long (e.g. 2s, 500ms, 1m); required with --wait-for.
        #[arg(long = "at-most", value_name = "DURATION", value_parser = parse_duration)]
        at_most: Option<Duration>,
        /// Commit once the files have been quiet for this long (gives up after 5s).
        /// Combine with --wait-for to wait for the line first, then for quiet.
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        settle: Option<Duration>,
        /// Poll interval while waiting.
        #[arg(long, value_name = "DURATION", value_parser = parse_duration, default_value = "20ms")]
        interval: Duration,
        #[arg(value_name = "FILE", add = ArgValueCompleter::new(complete_session_files))]
        files: Vec<String>,
    },
    /// Fold the uncommitted lines into the most recent checkpoint (amend).
    #[command(alias = "sq")]
    Squash {
        #[arg(value_name = "FILE", add = ArgValueCompleter::new(complete_session_files))]
        files: Vec<String>,
    },
    /// Revert the last commit.
    Undo,
    /// List the commit history (id, message, line counts).
    List,
    /// Per-file cursor + how many lines are unseen.
    Status,
    /// Empty the session in place: re-baseline cursors to EOF and drop history (keeps watching the files).
    Clear,
}

/// Dynamic completion: the short names of the files in the current session.
fn complete_session_files(_current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok((state, _)) = load_state() else {
        return Vec::new();
    };
    state
        .files
        .iter()
        .map(|f| CompletionCandidate::new(short(&f.path)))
        .collect()
}

/// Dynamic completion: checkpoint refs from the session — each checkpoint by message
/// (falling back to its id) plus its `^N` from-the-end ref.
fn complete_checkpoints(_current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok((state, _)) = load_state() else {
        return Vec::new();
    };
    let n = state.history.len();
    state
        .history
        .iter()
        .enumerate()
        .flat_map(|(i, c)| {
            let primary = match &c.message {
                Some(m) => CompletionCandidate::new(m),
                None => CompletionCandidate::new(c.id.to_string()),
            };
            [primary, CompletionCandidate::new(format!("^{}", n - i))]
        })
        .collect()
}

/// Parse a short duration like `2s`, `500ms`, or `1m` (units: ms, s, m).
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .ok_or_else(|| format!("missing unit in '{s}' (use ms, s, or m)"))?;
    let (num, unit) = s.split_at(split);
    let n: f64 = num
        .parse()
        .map_err(|_| format!("invalid number in '{s}'"))?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        other => return Err(format!("unknown unit '{other}' in '{s}' (use ms, s, or m)")),
    };
    Ok(Duration::from_secs_f64(secs))
}

/// Construct the best available [`Notify`] backend, or `None` if none is
/// available (falls back to polling). Linux: inotify; other platforms: `None`.
fn new_notify() -> Option<Box<dyn Notify>> {
    #[cfg(target_os = "linux")]
    {
        InotifyNotify::new()
            .ok()
            .map(|n| Box::new(n) as Box<dyn Notify>)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// inotify-backed [`Notify`] for Linux. Watches `IN_MODIFY | IN_DELETE_SELF |
/// IN_MOVE_SELF` per file. `wait` drains the event queue (we only care that
/// *something* changed — the caller re-stats to find out what).
#[cfg(target_os = "linux")]
struct InotifyNotify {
    fd: i32,
    watches: std::cell::RefCell<std::collections::HashMap<String, i32>>,
}

#[cfg(target_os = "linux")]
impl InotifyNotify {
    fn new() -> std::io::Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(InotifyNotify {
            fd,
            watches: std::cell::RefCell::new(std::collections::HashMap::new()),
        })
    }
}

#[cfg(target_os = "linux")]
impl Notify for InotifyNotify {
    fn watch(&self, path: &str) {
        let mask = libc::IN_MODIFY | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;
        // Remove the old watch (if any) so the new inode gets a fresh wd.
        if let Some(old_wd) = self.watches.borrow_mut().remove(path) {
            unsafe { libc::inotify_rm_watch(self.fd, old_wd) };
        }
        let c_path = std::ffi::CString::new(path).unwrap();
        let wd = unsafe { libc::inotify_add_watch(self.fd, c_path.as_ptr(), mask) };
        if wd >= 0 {
            self.watches.borrow_mut().insert(path.to_string(), wd);
        }
    }

    fn wait(&self, timeout: Duration) {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let _ = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        // Drain pending events so the next poll blocks until a *new* change.
        if pfd.revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 4096];
            unsafe {
                libc::read(self.fd, buf.as_mut_ptr() as *mut _, buf.len());
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for InotifyNotify {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

fn run(cmd: Cmd) -> Result<(), String> {
    let style = Style::new(color::enabled());
    match cmd {
        Cmd::Open { from_start, files } => {
            let paths: Vec<String> = files.iter().map(|f| abspath(f)).collect();
            let mut err = io::stderr();
            let state = open(&OsFs, &paths, from_start, style, &mut err);
            let path = state_path();
            save_state(&state, &path)?;
            let _ = writeln!(err, "session: {}", style.dim(&path.display().to_string()));
            Ok(())
        }
        Cmd::Diff {
            prefix,
            follow,
            interval,
            in_ref,
            files,
        } => {
            let (state, _) = load_state()?;
            let mut out = io::stdout().lock();
            let mut err = io::stderr();
            match (follow, in_ref) {
                (true, Some(_)) => Err("--follow and --in are mutually exclusive".into()),
                (true, None) => {
                    let notify = new_notify();
                    let clock = OsClock::new();
                    diff_follow(
                        &state,
                        &OsFs,
                        notify.as_deref(),
                        &clock,
                        &files,
                        prefix,
                        interval,
                        style,
                        &mut out,
                        &mut err,
                    )
                }
                (false, Some(at)) => {
                    diff_in(&state, &OsFs, &at, &files, prefix, style, &mut out, &mut err)
                }
                (false, None) => diff(&state, &OsFs, &files, prefix, style, &mut out, &mut err),
            }
        }
        Cmd::Commit {
            message,
            wait_for,
            at_most,
            settle,
            interval,
            files,
        } => {
            let (mut state, path) = load_state()?;
            // On timeout/non-settle these return Err, so `?` skips save_state — the
            // session stays untouched (abort, don't commit).
            let fs = &OsFs;
            let clock = &OsClock::new();
            let err = &mut io::stderr();
            match (wait_for, settle) {
                (Some(needle), Some(dur)) => commit_wait_settle(
                    &mut state,
                    fs,
                    clock,
                    &files,
                    &needle,
                    at_most.expect("clap requires --at-most with --wait-for"),
                    dur,
                    interval,
                    message,
                    style,
                    err,
                )?,
                (Some(needle), None) => commit_wait(
                    &mut state,
                    fs,
                    clock,
                    &files,
                    &needle,
                    at_most.expect("clap requires --at-most with --wait-for"),
                    interval,
                    message,
                    style,
                    err,
                )?,
                (None, Some(dur)) => {
                    commit_settle(&mut state, fs, clock, &files, dur, interval, message, style, err)?
                }
                (None, None) => commit(&mut state, fs, clock, &files, message, style, err)?,
            }
            save_state(&state, &path)
        }
        Cmd::Squash { files } => {
            let (mut state, path) = load_state()?;
            squash(&mut state, &OsFs, &files, style, &mut io::stderr())?;
            save_state(&state, &path)
        }
        Cmd::Undo => {
            let (mut state, path) = load_state()?;
            undo(&mut state, style, &mut io::stderr());
            save_state(&state, &path)
        }
        Cmd::List => {
            let (state, spath) = load_state()?;
            list(
                &state,
                &OsFs,
                &spath.display().to_string(),
                style,
                &mut io::stderr(),
            );
            Ok(())
        }
        Cmd::Status => {
            let (state, spath) = load_state()?;
            status(
                &state,
                &OsFs,
                &spath.display().to_string(),
                style,
                &mut io::stderr(),
            );
            Ok(())
        }
        Cmd::Clear => {
            let (mut state, path) = load_state()?;
            clear(&mut state, &OsFs, style, &mut io::stderr());
            save_state(&state, &path)
        }
    }
}

fn main() -> ExitCode {
    CompleteEnv::with_factory(Cli::command).complete();
    let cli = Cli::parse();
    match run(cli.cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("logsnap: {e}");
            ExitCode::FAILURE
        }
    }
}
