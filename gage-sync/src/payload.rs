use std::path::{Path, PathBuf};

use gage_core::config::gage_home;

/// One unit of transfer: a local path (file or directory) mapped to a
/// relative remote path under the backend's root.
#[derive(Debug, Clone)]
pub struct TransferItem {
    pub local: PathBuf,
    pub remote: String,
}

/// Builds the push payload from local state.
///
/// `db_snapshot` is a path to a consistent copy of `gage.db` produced via
/// `VACUUM INTO` before the call. The original live db is never read here.
pub fn build_payload(db_snapshot: &Path) -> Vec<TransferItem> {
    let gh = gage_home();
    let mut items = Vec::new();

    items.push(TransferItem {
        local: db_snapshot.to_path_buf(),
        remote: "gage/data/gage.db".to_string(),
    });

    let cfg = gh.join("config.toml");
    if cfg.is_file() {
        items.push(TransferItem {
            local: cfg,
            remote: "gage/config.toml".to_string(),
        });
    }

    let evals = gh.join("evals");
    if evals.is_dir() {
        items.push(TransferItem {
            local: evals,
            remote: "gage/evals".to_string(),
        });
    }

    if let Some(claude_projects) = claude_projects_dir()
        && claude_projects.is_dir()
    {
        items.push(TransferItem {
            local: claude_projects,
            remote: "claude/projects".to_string(),
        });
    }

    items
}

fn claude_projects_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude").join("projects"))
}
