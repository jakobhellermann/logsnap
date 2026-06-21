//! Pure line/cursor math: deciding where to read from (rotation/truncation
//! detection) and splitting raw bytes into complete lines vs. a trailing partial.

use crate::fs::Stat;
use crate::state::FileState;

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Event {
    Ok,
    Missing,     // file not present (and we have no prior identity)
    Disappeared, // we had it, now it's gone
    Appeared,    // we had no identity, now it's there
    Rotated,     // identity changed: replaced/rotated
    Truncated,   // same identity but shrank below the cursor
}

impl Event {
    pub fn absent(self) -> bool {
        matches!(self, Event::Missing | Event::Disappeared)
    }
}

/// Decide where to read from, given the current on-disk state. Pure.
pub fn resolve(f: &FileState, st: &Option<Stat>) -> (u64, Event) {
    match st {
        None => {
            if f.ino == 0 {
                (0, Event::Missing)
            } else {
                (f.cursor, Event::Disappeared)
            }
        }
        Some(s) => {
            if f.ino == 0 {
                // Never had an identity (opened while absent): adopt, read from cursor.
                (f.cursor.min(s.size), Event::Appeared)
            } else if s.dev != f.dev || s.ino != f.ino {
                (0, Event::Rotated)
            } else if s.size < f.cursor {
                (0, Event::Truncated)
            } else {
                (f.cursor, Event::Ok)
            }
        }
    }
}

pub struct Region {
    /// Complete lines (everything up to and including the last '\n'), raw bytes.
    pub bytes: Vec<u8>,
    /// Absolute byte offset just past the last complete line.
    pub end: u64,
    /// Number of complete lines.
    pub lines: usize,
    /// Bytes of a trailing partial (no newline yet) line, left uncommitted.
    pub partial: usize,
}

/// The new complete lines in `bytes` starting at absolute offset `from`.
pub fn region(bytes: &[u8], from: u64) -> Region {
    let start = (from as usize).min(bytes.len());
    let slice = &bytes[start..];
    match slice.iter().rposition(|&b| b == b'\n') {
        Some(idx) => {
            let complete = slice[..=idx].to_vec();
            let lines = complete.iter().filter(|&&b| b == b'\n').count();
            Region {
                bytes: complete,
                end: from + idx as u64 + 1,
                lines,
                partial: slice.len() - (idx + 1),
            }
        }
        None => Region {
            bytes: Vec::new(),
            end: from,
            lines: 0,
            partial: slice.len(),
        },
    }
}

/// `(lines before `from`, total lines)` — for the line-based status display.
pub fn line_stats(bytes: &[u8], from: u64) -> (usize, usize) {
    let total = bytes.iter().filter(|&&b| b == b'\n').count();
    let cut = (from as usize).min(bytes.len());
    let before = bytes[..cut].iter().filter(|&&b| b == b'\n').count();
    (before, total)
}
