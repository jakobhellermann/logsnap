//! logsnap CLI — thin clap wrapper that wires the real filesystem ([`OsFs`]) and
//! stdout/stderr into the library command functions, plus session load/save and
//! dynamic shell completion.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::CompleteEnv;
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};

use logsnap::*;

#[derive(Parser)]
#[command(
    name = "logsnap",
    about = "cursor-based log snapshotting (multi-file, rotation-aware)",
    after_help = "Content goes to stdout; headers/warnings to stderr — so `logsnap show | grep X` \
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
        #[arg(required = true, value_name = "FILE")]
        files: Vec<String>,
    },
    /// Print the new lines since the cursor (READ-ONLY, repeatable).
    Show {
        /// Prefix each line with the file name (attribution across files).
        #[arg(short, long)]
        prefix: bool,
        #[arg(value_name = "FILE", add = ArgValueCompleter::new(complete_session_files))]
        files: Vec<String>,
    },
    /// Commit past the new lines (snapshots for undo).
    Commit {
        #[arg(value_name = "FILE", add = ArgValueCompleter::new(complete_session_files))]
        files: Vec<String>,
    },
    /// Revert the last commit.
    Undo,
    /// Per-file cursor + how many lines are unseen.
    Status,
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

fn run(cmd: Cmd) -> Result<(), String> {
    match cmd {
        Cmd::Open { from_start, files } => {
            let paths: Vec<String> = files.iter().map(|f| abspath(f)).collect();
            let mut err = io::stderr();
            let state = open(&OsFs, &paths, from_start, &mut err);
            let path = state_path();
            save_state(&state, &path)?;
            let _ = writeln!(err, "session: {}", path.display());
            Ok(())
        }
        Cmd::Show { prefix, files } => {
            let (state, _) = load_state()?;
            show(
                &state,
                &OsFs,
                &files,
                prefix,
                &mut io::stdout().lock(),
                &mut io::stderr(),
            )
        }
        Cmd::Commit { files } => {
            let (mut state, path) = load_state()?;
            commit(&mut state, &OsFs, &files, &mut io::stderr())?;
            save_state(&state, &path)
        }
        Cmd::Undo => {
            let (mut state, path) = load_state()?;
            undo(&mut state, &mut io::stderr());
            save_state(&state, &path)
        }
        Cmd::Status => {
            let (state, spath) = load_state()?;
            status(
                &state,
                &OsFs,
                &spath.display().to_string(),
                &mut io::stderr(),
            );
            Ok(())
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
