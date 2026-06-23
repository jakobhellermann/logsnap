//! logsnap core — cursor-based log snapshotting, decoupled from the filesystem and
//! from stdout/stderr so it can be unit-tested in-process.
//!
//! - [`fs`] — the `Fs` backend (real `OsFs` / in-memory `MemFs`).
//! - [`cursor`] — pure line/cursor math (rotation/truncation detection, line splitting).
//! - [`clock`] — the `Clock` backend (real `OsClock` / a virtual one in tests).
//! - [`color`] — minimal ANSI color helpers (headers/warnings only, never content).
//! - [`state`] — the persisted session.
//! - [`commands`] — `open`/`diff`/`commit`/`undo`/`list`/`status`, over any `Fs` and `Write`.
//! - [`session`] — on-disk session persistence (binary only).

pub mod clock;
pub mod color;
pub mod commands;
pub mod cursor;
pub mod fs;
pub mod session;
pub mod state;

pub use clock::*;
pub use color::*;
pub use commands::*;
pub use cursor::*;
pub use fs::*;
pub use session::*;
pub use state::*;
