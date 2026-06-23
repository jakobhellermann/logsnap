//! Minimal ANSI color helpers. Color is used only on stderr (headers/warnings) —
//! content (stdout) is never colored. Suppressed when `NO_COLOR` is set or the
//! stream is not a TTY.

use std::io::IsTerminal;

/// Whether ANSI colors should be used: TTY check + respect `NO_COLOR`.
pub fn enabled() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// A coloring context: either on (apply ANSI codes) or off (pass through).
/// Constructed once in `main.rs` and threaded through the command functions.
#[derive(Clone, Copy)]
pub struct Style {
    color: bool,
}

impl Style {
    pub fn new(color: bool) -> Self {
        Style { color }
    }

    /// Dim/grey — for headers, meta info.
    pub fn dim(self, s: &str) -> String {
        if self.color {
            format!("{DIM}{s}{RESET}")
        } else {
            s.to_string()
        }
    }

    /// Bold — for commit summaries, emphasis.
    pub fn bold(self, s: &str) -> String {
        if self.color {
            format!("{BOLD}{s}{RESET}")
        } else {
            s.to_string()
        }
    }

    /// Cyan — for file names in headers.
    pub fn cyan(self, s: &str) -> String {
        if self.color {
            format!("{CYAN}{s}{RESET}")
        } else {
            s.to_string()
        }
    }

    /// Yellow — for truncation/rotation warnings.
    pub fn yellow(self, s: &str) -> String {
        if self.color {
            format!("{YELLOW}{s}{RESET}")
        } else {
            s.to_string()
        }
    }

    /// Red — for errors.
    pub fn red(self, s: &str) -> String {
        if self.color {
            format!("{RED}{s}{RESET}")
        } else {
            s.to_string()
        }
    }
}
