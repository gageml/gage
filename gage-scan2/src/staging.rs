//! The staging directory of a running scan.
//!
//! A scan is a transaction: while it runs, everything it records goes
//! to `staging/<scan_id>/` under Gage home, and nothing is written to
//! the store. At the terminal state the `scan/` subtree is applied to
//! the store as the scan object's content, so `scan/` mirrors the
//! object tree exactly (see `gage_store::ScanStore`). Layout:
//!
//! ```text
//! state                                  # running | completed | canceled
//! pid                                    # writer process, present while running
//! applied                                # present once written to the store
//! scan/attrs.json                        # written at the terminal state
//! scan/tasks/<scanner>/<task>/attrs.json # pending at create, rewritten on start and finish
//! scan/tasks/<scanner>/<task>/error.txt  # failed tasks only
//! ```
//!
//! Every file is written whole through a temp file and a rename, so a
//! crash never leaves a torn file. A directory left behind by a crash
//! stays in `running` with a dead pid; recovery is not implemented
//! yet.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gage_store::{ScanAttrs, TaskAttrs, TaskStatus};
use serde::Serialize;

const STATE_FILE: &str = "state";
const PID_FILE: &str = "pid";
const APPLIED_FILE: &str = "applied";
const SCAN_DIR: &str = "scan";
const TASKS_DIR: &str = "tasks";
const ATTRS_FILE: &str = "attrs.json";
const ERROR_FILE: &str = "error.txt";

/// The staging root under Gage home.
pub fn staging_root() -> PathBuf {
    gage_core::config::gage_home().join("staging")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Completed,
    Canceled,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Completed => "completed",
            State::Canceled => "canceled",
        }
    }
}

/// One scan's staging directory.
pub struct Staging {
    dir: PathBuf,
}

impl Staging {
    /// Create `root/<scan_id>/` in the `running` state with this
    /// process's pid and one `pending` task record per `(scanner,
    /// task)`.
    pub fn create(root: &Path, scan_id: &str, tasks: &[(String, String)]) -> io::Result<Staging> {
        let dir = root.join(scan_id);
        fs::create_dir_all(dir.join(SCAN_DIR))?;
        let staging = Staging { dir };
        write_atomic(
            &staging.dir.join(PID_FILE),
            format!("{}\n", std::process::id()).as_bytes(),
        )?;
        for (scanner, task) in tasks {
            staging.write_task(
                scanner,
                task,
                &TaskAttrs {
                    status: TaskStatus::Pending,
                    started: None,
                    stopped: None,
                    worked_ms: None,
                },
            )?;
        }
        staging.set_state(State::Running)?;
        Ok(staging)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The `scan/` subtree, the content applied to the store.
    pub fn scan_dir(&self) -> PathBuf {
        self.dir.join(SCAN_DIR)
    }

    pub fn set_state(&self, state: State) -> io::Result<()> {
        write_atomic(
            &self.dir.join(STATE_FILE),
            format!("{}\n", state.as_str()).as_bytes(),
        )
    }

    /// Write a task's `attrs.json`, replacing any prior record.
    pub fn write_task(&self, scanner: &str, task: &str, attrs: &TaskAttrs) -> io::Result<()> {
        let dir = self.task_dir(scanner, task);
        fs::create_dir_all(&dir)?;
        write_json(&dir.join(ATTRS_FILE), attrs)
    }

    /// Write a failed task's `error.txt`.
    pub fn write_task_error(&self, scanner: &str, task: &str, message: &str) -> io::Result<()> {
        write_atomic(
            &self.task_dir(scanner, task).join(ERROR_FILE),
            message.as_bytes(),
        )
    }

    /// Write the scan's `attrs.json`.
    pub fn write_scan(&self, attrs: &ScanAttrs) -> io::Result<()> {
        write_json(&self.scan_dir().join(ATTRS_FILE), attrs)
    }

    /// Record that the scan has been written to the store.
    pub fn mark_applied(&self) -> io::Result<()> {
        write_atomic(&self.dir.join(APPLIED_FILE), b"")
    }

    /// Remove the directory. Called after `mark_applied`.
    pub fn remove(self) -> io::Result<()> {
        fs::remove_dir_all(&self.dir)
    }

    fn task_dir(&self, scanner: &str, task: &str) -> PathBuf {
        self.scan_dir().join(TASKS_DIR).join(scanner).join(task)
    }
}

/// Write `value` as one-line JSON with a trailing newline, the
/// encoding the store uses for `attrs.json`.
fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut json = serde_json::to_string(value).map_err(io::Error::other)?;
    json.push('\n');
    write_atomic(path, json.as_bytes())
}

/// Write `bytes` to a sibling temp file and rename it over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use gage_store::{DirFiles, ScanContent, TaskCounts};

    use super::*;

    #[test]
    fn create_records_the_plan_and_writes_round_trip_through_the_store_decoder() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks = vec![
            ("hello".to_string(), "greet".to_string()),
            ("hello".to_string(), "fail".to_string()),
        ];
        let staging = Staging::create(tmp.path(), "SCAN1", &tasks).unwrap();
        assert_eq!(staging.dir(), tmp.path().join("SCAN1"));
        assert_eq!(
            fs::read_to_string(staging.dir().join("state")).unwrap(),
            "running\n"
        );
        assert_eq!(
            fs::read_to_string(staging.dir().join("pid")).unwrap(),
            format!("{}\n", std::process::id())
        );

        staging
            .write_task(
                "hello",
                "fail",
                &TaskAttrs {
                    status: TaskStatus::Failed,
                    started: Some(1),
                    stopped: Some(2),
                    worked_ms: None,
                },
            )
            .unwrap();
        staging.write_task_error("hello", "fail", "boom\n").unwrap();
        let attrs = ScanAttrs {
            started: 1,
            stopped: 2,
            canceled: false,
            tasks: TaskCounts {
                total: 2,
                completed: 0,
                failed: 1,
                skipped: 0,
            },
        };
        staging.write_scan(&attrs).unwrap();
        staging.set_state(State::Completed).unwrap();

        let content = ScanContent::from_files(&DirFiles::new(&staging.scan_dir())).unwrap();
        assert_eq!(content.attrs, attrs);
        assert_eq!(content.tasks.len(), 2);
        assert_eq!(content.tasks[0].task, "fail");
        assert_eq!(content.tasks[0].attrs.status, TaskStatus::Failed);
        assert_eq!(content.tasks[0].error.as_deref(), Some("boom\n"));
        assert_eq!(content.tasks[1].task, "greet");
        assert_eq!(content.tasks[1].attrs.status, TaskStatus::Pending);
        assert!(
            !staging.scan_dir().join("attrs.json.tmp").exists(),
            "temp file is renamed away"
        );

        staging.mark_applied().unwrap();
        let dir = staging.dir().to_path_buf();
        assert!(dir.join("applied").exists());
        staging.remove().unwrap();
        assert!(!dir.exists());
    }
}
