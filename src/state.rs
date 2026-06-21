//! The persisted session: per-file cursors and the undo stack.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct FileState {
    pub path: String,
    #[serde(default)]
    pub dev: u64,
    #[serde(default)]
    pub ino: u64,
    /// Byte offset of the committed read position (start of the next unseen line).
    #[serde(default)]
    pub cursor: u64,
}

/// One entry per file, captured before an `advance`, so `undo` can restore it.
#[derive(Serialize, Deserialize, Clone)]
pub struct Snap {
    pub cursor: u64,
    pub dev: u64,
    pub ino: u64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    /// Open files, in the order they were passed to `open`.
    pub files: Vec<FileState>,
    /// Stack of pre-advance snapshots (newest last). Each is one Snap per file.
    #[serde(default)]
    pub undo: Vec<Vec<Snap>>,
}
