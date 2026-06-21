//! On-disk session persistence (used by the binary; tests drive [`State`] directly).

use std::fs;
use std::path::{Path, PathBuf};

use crate::state::State;

/// The canonical state file path: `$LOGSNAP_STATE` if set, else
/// `$XDG_STATE_HOME/logsnap/state.json` (XDG default `~/.local/state`).
pub fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("LOGSNAP_STATE") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".local").join("state")
        });
    base.join("logsnap").join("state.json")
}

pub fn find_state() -> Option<PathBuf> {
    let p = state_path();
    p.exists().then_some(p)
}

pub fn load_state() -> Result<(State, PathBuf), String> {
    let path = find_state().ok_or("no logsnap session. Start one with: logsnap open <files...>")?;
    let data = fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let state: State =
        serde_json::from_str(&data).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    Ok((state, path))
}

pub fn save_state(state: &State, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    fs::write(path, data).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Make a path absolute without requiring it to exist (canonicalize would fail on a
/// not-yet-created log), collapsing `.`/`..` lexically.
pub fn abspath(s: &str) -> String {
    let p = PathBuf::from(s);
    let abs = if p.is_absolute() {
        p
    } else {
        std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p)
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        use std::path::Component::*;
        match comp {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other),
        }
    }
    out.to_string_lossy().into_owned()
}
