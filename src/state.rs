//! The persisted session: per-file cursors and the commit history.

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

/// What one file contributed to a commit: the byte range that was committed (so it
/// can be re-read by `show --at`, as long as the file's identity still matches) plus
/// the prior cursor state (so `undo` can restore it). No content is stored.
#[derive(Serialize, Deserialize, Clone)]
pub struct CommitEntry {
    pub path: String,
    /// The committed slice `[from, to)` and the file identity at commit time.
    pub from: u64,
    pub to: u64,
    pub dev: u64,
    pub ino: u64,
    pub lines: usize,
    /// Cursor state before the commit, for `undo`.
    pub prev_cursor: u64,
    pub prev_dev: u64,
    pub prev_ino: u64,
}

/// A checkpoint (optionally with a message): the lines committed across one or more
/// files. The message doubles as a human label and a `diff --in <message>` lookup ref.
#[derive(Serialize, Deserialize, Clone)]
pub struct Commit {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Local wall-clock time of day the checkpoint was created, `HH:MM:SS` (see
    /// [`Clock::now_hms`](crate::clock::Clock::now_hms)). `None` for pre-timestamp sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub entries: Vec<CommitEntry>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    /// Open files, in the order they were passed to `open`.
    pub files: Vec<FileState>,
    /// Commit history (oldest first). `undo` pops the last; `show --at` re-reads any.
    #[serde(default)]
    pub history: Vec<Commit>,
    /// Next checkpoint id to assign (monotonic; reused when the last commit is undone).
    #[serde(default)]
    pub next_id: u32,
}
