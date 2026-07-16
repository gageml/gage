use std::fs;
use std::io;
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
/// `slow_db_snapshot` is the same for `log/slow.db`, or `None` when there
/// is no slow query log.
pub fn build_payload(
    db_snapshot: &Path,
    slow_db_snapshot: Option<&Path>,
) -> io::Result<Vec<TransferItem>> {
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

    let claude = gh.join("claude");
    if claude.is_dir() {
        items.push(TransferItem {
            local: claude,
            remote: "gage/claude".to_string(),
        });
    }

    if let Some(snap) = slow_db_snapshot {
        items.push(TransferItem {
            local: snap.to_path_buf(),
            remote: "gage/log/slow.db".to_string(),
        });
    }

    let log = gh.join("log");
    if log.is_dir() {
        for entry in fs::read_dir(&log)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Live sqlite files; the consistent snapshot above covers them
            if name.starts_with("slow.db") {
                continue;
            }
            items.push(TransferItem {
                local: entry.path(),
                remote: format!("gage/log/{name}"),
            });
        }
    }

    if let Some(claude_projects) = gage_claude::session::projects_dir()
        && claude_projects.is_dir()
    {
        items.push(TransferItem {
            local: claude_projects,
            remote: "claude/projects".to_string(),
        });
    }

    Ok(items)
}
