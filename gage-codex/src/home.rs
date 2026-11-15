//! Where Codex keeps its state.

use std::env;
use std::path::PathBuf;

/// The Codex home: `CODEX_HOME` when set, else `.codex` under the
/// user's home directory. `None` when neither is set.
pub(crate) fn codex_home() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("CODEX_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex"))
}
