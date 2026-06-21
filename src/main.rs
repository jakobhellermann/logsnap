//! logsnap CLI — thin wrapper that wires the real filesystem ([`OsFs`]) and
//! stdout/stderr into the library command functions, plus session load/save.

use std::io::{self, Write};
use std::process::ExitCode;

use logsnap::*;

fn cmd_open(args: &[String]) -> Result<(), String> {
    let mut from_start = false;
    let mut paths = Vec::new();
    for a in args {
        match a.as_str() {
            "--from-start" | "-s" => from_start = true,
            _ => paths.push(abspath(a)),
        }
    }
    if paths.is_empty() {
        return Err("open: need at least one file".into());
    }
    let mut err = io::stderr();
    let state = open(&OsFs, &paths, from_start, &mut err);
    let path = state_path();
    save_state(&state, &path)?;
    let _ = writeln!(err, "session: {}", path.display());
    Ok(())
}

fn cmd_show(args: &[String]) -> Result<(), String> {
    let mut prefix = false;
    let mut names = Vec::new();
    for a in args {
        match a.as_str() {
            "--prefix" | "-p" => prefix = true,
            _ => names.push(a.clone()),
        }
    }
    let (state, _) = load_state()?;
    show(
        &state,
        &OsFs,
        &names,
        prefix,
        &mut io::stdout().lock(),
        &mut io::stderr(),
    )
}

fn cmd_advance(args: &[String]) -> Result<(), String> {
    let (mut state, path) = load_state()?;
    advance(&mut state, &OsFs, args, &mut io::stderr())?;
    save_state(&state, &path)
}

fn cmd_undo() -> Result<(), String> {
    let (mut state, path) = load_state()?;
    undo(&mut state, &mut io::stderr());
    save_state(&state, &path)
}

fn cmd_status() -> Result<(), String> {
    let (state, spath) = load_state()?;
    status(
        &state,
        &OsFs,
        &spath.display().to_string(),
        &mut io::stderr(),
    );
    Ok(())
}

fn usage() {
    eprintln!(
        "logsnap — cursor-based log snapshotting (multi-file, rotation-aware)

USAGE:
  logsnap open [--from-start] <file>...   start a session; cursors at EOF (or 0)
  logsnap show [--prefix] [file]...       print new lines since cursor (READ-ONLY, repeatable)
  logsnap advance [file]...               commit past the new lines (snapshots for undo)
  logsnap undo                            revert the last advance
  logsnap status | view                   per-file cursor + how many lines are unseen

Content goes to stdout; headers/warnings to stderr — so `logsnap show | grep X`
filters only content and never swallows an identity-change warning.

State: $XDG_STATE_HOME/logsnap/state.json (~/.local/state/...); override with $LOGSNAP_STATE."
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            usage();
            return ExitCode::FAILURE;
        }
    };
    let res = match cmd {
        "open" => cmd_open(rest),
        "show" => cmd_show(rest),
        "advance" | "adv" => cmd_advance(rest),
        "undo" => cmd_undo(),
        "status" | "view" | "st" => cmd_status(),
        "help" | "-h" | "--help" => {
            usage();
            return ExitCode::SUCCESS;
        }
        other => Err(format!("unknown command: {other}\n(try: logsnap help)")),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("logsnap: {e}");
            ExitCode::FAILURE
        }
    }
}
