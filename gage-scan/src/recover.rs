//! Recovery of scan directories a crashed run left behind.
//!
//! A run that dies leaves its directory under `scans/` in one of three
//! states: `running` with a dead writer, a terminal state that was
//! never applied, or applied but not removed. Each is recovered the
//! way the run would have ended: a dead run is applied as canceled
//! with whatever it produced, an unapplied terminal run is applied as
//! it stands, and an applied directory is removed.

use std::io;
use std::path::Path;

use gage_core::datetime::now_ms;
use gage_runtime::CURRENT_RUNTIME_SCHEME;
use gage_store::{ScanAttrs, Store, TaskAttrs, TaskCounts, TaskStatus};

use crate::scan_dir::{ScanDir, State};
use crate::{ScanError, apply_scan_dir};

/// A scan directory no live run owns.
pub struct Orphan {
    scan_dir: ScanDir,
    pub reason: OrphanReason,
}

impl Orphan {
    pub fn id(&self) -> &str {
        self.scan_dir.id()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanReason {
    /// The state is `running` and the writer is gone; `pid` is the
    /// writer's pid when the file was written
    DeadWriter { pid: Option<u32> },
    /// The run ended in `state` but was never applied
    Unapplied(State),
    /// The run was applied and the directory was not removed
    Applied,
}

/// The scan directories under `scans_dir` no live run owns, in
/// directory order. Empty when `scans_dir` does not exist. One
/// `read_dir` when it is empty; each directory costs a state, pid,
/// and applied-marker read.
pub fn find_orphans(scans_dir: &Path) -> io::Result<Vec<Orphan>> {
    let entries = match std::fs::read_dir(scans_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut dirs: Vec<_> = entries
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<_>>()?;
    dirs.sort();
    let mut orphans = Vec::new();
    for dir in dirs.into_iter().filter(|d| d.is_dir()) {
        let scan_dir = ScanDir::open(&dir)?;
        if let Some(reason) = orphan_reason(&scan_dir)? {
            orphans.push(Orphan { scan_dir, reason });
        }
    }
    Ok(orphans)
}

/// Why `scan_dir` is an orphan, or `None` when a live run owns it. A
/// directory without a state file is one whose writer died during
/// create, so it is judged by its pid like a running one.
fn orphan_reason(scan_dir: &ScanDir) -> io::Result<Option<OrphanReason>> {
    if scan_dir.is_applied() {
        return Ok(Some(OrphanReason::Applied));
    }
    match scan_dir.state()? {
        Some(state) if state.is_terminal() => Ok(Some(OrphanReason::Unapplied(state))),
        _ => {
            let pid = scan_dir.pid()?;
            match pid {
                Some(pid) if pid_alive(pid) => Ok(None),
                _ => Ok(Some(OrphanReason::DeadWriter { pid })),
            }
        }
    }
}

/// True when a process with `pid` exists. Signal 0 delivers nothing
/// and reports whether the target exists; `EPERM` is an existing
/// process owned by someone else.
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill with signal 0 has no effect on the target process
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// What recovery did with an orphan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovered {
    /// The scan was applied to the store under `commit_sha`
    Applied {
        commit_sha: String,
        attrs: ScanAttrs,
    },
    /// The directory was already applied and is now removed
    Removed,
}

/// Recover `orphan`: end a dead run as canceled, apply what the
/// directory holds, and remove it. `gage_version` is recorded as the
/// runtime of a run that never wrote its own attrs.
pub fn recover(store: &Store, orphan: Orphan, gage_version: &str) -> Result<Recovered, ScanError> {
    let Orphan { scan_dir, reason } = orphan;
    let attrs = match reason {
        OrphanReason::Applied => {
            scan_dir.remove()?;
            return Ok(Recovered::Removed);
        }
        OrphanReason::DeadWriter { pid } => cancel_dead_run(&scan_dir, pid, gage_version)?,
        OrphanReason::Unapplied(_) => scan_dir.scan_attrs()?,
    };
    let commit_sha = apply_scan_dir(store, &scan_dir)?;
    scan_dir.remove()?;
    Ok(Recovered::Applied { commit_sha, attrs })
}

/// Bring a dead run to the canceled state the run itself would have
/// reached: every task without a terminal status is recorded
/// canceled, the scan's attrs are written with `canceled: true`, and
/// the error log says why.
fn cancel_dead_run(
    scan_dir: &ScanDir,
    pid: Option<u32>,
    gage_version: &str,
) -> Result<ScanAttrs, ScanError> {
    let stopped = now_ms();
    let mut counts = TaskCounts::default();
    for task in scan_dir.tasks()? {
        counts.total += 1;
        match task.attrs.status {
            TaskStatus::Completed => counts.completed += 1,
            TaskStatus::Failed => counts.failed += 1,
            TaskStatus::Canceled => {}
            TaskStatus::Pending | TaskStatus::Started => {
                scan_dir.write_task(
                    &task.scanner,
                    &task.task,
                    &TaskAttrs {
                        status: TaskStatus::Canceled,
                        started: task.attrs.started,
                        stopped: task.attrs.started.map(|_| stopped),
                        worked_ms: task.attrs.worked_ms,
                    },
                )?;
            }
        }
    }
    let attrs = ScanAttrs {
        runtime: format!("{CURRENT_RUNTIME_SCHEME} {gage_version}"),
        started: scan_dir.created_ms()?,
        stopped,
        canceled: true,
        tasks: counts,
    };
    let writer = match pid {
        Some(pid) => format!("scan process {pid}"),
        None => "scan process".to_string(),
    };
    scan_dir
        .scan_logs()
        .err(&format!("{writer} died; scan recovered as canceled\n"))?;
    scan_dir.write_scan(&attrs)?;
    scan_dir.set_state(State::Canceled)?;
    Ok(attrs)
}

#[cfg(test)]
mod tests {
    use gage_store::ScanStore;
    use tempfile::TempDir;

    use super::*;
    use crate::scan_dir::ScannerPlan;

    fn open_store() -> (TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open(&path).unwrap();
        (tmp, store)
    }

    /// A running scan directory with two pending tasks, owned by this
    /// process.
    fn running_scan_dir(scans_dir: &Path, id: &str) -> ScanDir {
        let tasks = ["one".to_string(), "two".to_string()];
        ScanDir::create(
            scans_dir,
            id,
            None,
            &[ScannerPlan {
                name: "hello",
                tasks: &tasks,
                sources: &[],
            }],
        )
        .unwrap()
    }

    /// A pid no process has: above the kernel's default `pid_max`.
    const DEAD_PID: u32 = 4_194_305;

    fn set_pid(scan_dir: &ScanDir, pid: u32) {
        std::fs::write(scan_dir.dir().join("pid"), format!("{pid}\n")).unwrap();
    }

    fn orphan_ids(scans_dir: &Path) -> Vec<(String, OrphanReason)> {
        find_orphans(scans_dir)
            .unwrap()
            .into_iter()
            .map(|o| (o.id().to_string(), o.reason))
            .collect()
    }

    #[test]
    fn a_missing_scans_dir_has_no_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_orphans(&tmp.path().join("scans")).unwrap().is_empty());
    }

    #[test]
    fn a_running_scan_owned_by_a_live_process_is_not_an_orphan() {
        let tmp = tempfile::tempdir().unwrap();
        let scans = tmp.path().join("scans");
        running_scan_dir(&scans, "LIVE");
        assert!(orphan_ids(&scans).is_empty());
    }

    #[test]
    fn a_dead_run_is_applied_as_canceled() {
        let (tmp, store) = open_store();
        let scans = tmp.path().join("scans");
        let scan_dir = running_scan_dir(&scans, "DEAD");
        // One task had started when the process died
        scan_dir
            .write_task(
                "hello",
                "one",
                &TaskAttrs {
                    status: TaskStatus::Started,
                    started: Some(1000),
                    stopped: None,
                    worked_ms: None,
                },
            )
            .unwrap();
        set_pid(&scan_dir, DEAD_PID);

        let orphans = orphan_ids(&scans);
        assert_eq!(
            orphans,
            [(
                "DEAD".to_string(),
                OrphanReason::DeadWriter {
                    pid: Some(DEAD_PID)
                }
            )]
        );

        let orphan = find_orphans(&scans).unwrap().pop().unwrap();
        let recovered = recover(&store, orphan, "test-version").unwrap();
        let Recovered::Applied { attrs, .. } = recovered else {
            panic!("a dead run is applied: {recovered:?}");
        };
        assert!(attrs.canceled);
        assert_eq!(
            attrs.runtime,
            format!("{CURRENT_RUNTIME_SCHEME} test-version")
        );
        assert_eq!(
            attrs.tasks,
            TaskCounts {
                total: 2,
                completed: 0,
                failed: 0,
            }
        );
        assert!(!scans.join("DEAD").exists(), "the directory is removed");

        let record = ScanStore::from(&store).get("DEAD").unwrap();
        assert_eq!(record.content.attrs, attrs);
        let statuses: Vec<(String, TaskStatus, Option<i64>)> = record
            .content
            .tasks
            .iter()
            .map(|t| (t.task.clone(), t.attrs.status, t.attrs.started))
            .collect();
        assert_eq!(
            statuses,
            [
                ("one".to_string(), TaskStatus::Canceled, Some(1000)),
                ("two".to_string(), TaskStatus::Canceled, None),
            ]
        );
        let err_log = ScanStore::from(&store)
            .scan_log(&record.commit_sha, "err")
            .unwrap()
            .expect("a recovered run writes its error log");
        let err_log = String::from_utf8(err_log).unwrap();
        assert!(
            err_log.contains(&format!("scan process {DEAD_PID} died")),
            "the error log names the cause: {err_log:?}"
        );
    }

    #[test]
    fn an_unapplied_terminal_run_is_applied_as_it_stands() {
        let (tmp, store) = open_store();
        let scans = tmp.path().join("scans");
        let scan_dir = running_scan_dir(&scans, "DONE");
        for task in ["one", "two"] {
            scan_dir
                .write_task(
                    "hello",
                    task,
                    &TaskAttrs {
                        status: TaskStatus::Completed,
                        started: Some(1),
                        stopped: Some(2),
                        worked_ms: Some(1),
                    },
                )
                .unwrap();
        }
        let attrs = ScanAttrs {
            runtime: "gage test".to_string(),
            started: 1,
            stopped: 2,
            canceled: false,
            tasks: TaskCounts {
                total: 2,
                completed: 2,
                failed: 0,
            },
        };
        scan_dir.write_scan(&attrs).unwrap();
        scan_dir.set_state(State::Completed).unwrap();
        set_pid(&scan_dir, DEAD_PID);

        assert_eq!(
            orphan_ids(&scans),
            [(
                "DONE".to_string(),
                OrphanReason::Unapplied(State::Completed)
            )]
        );
        let orphan = find_orphans(&scans).unwrap().pop().unwrap();
        let recovered = recover(&store, orphan, "test-version").unwrap();
        let Recovered::Applied { attrs: applied, .. } = recovered else {
            panic!("a terminal run is applied: {recovered:?}");
        };
        assert_eq!(applied, attrs);
        assert_eq!(
            ScanStore::from(&store).get("DONE").unwrap().content.attrs,
            attrs
        );
        assert!(!scans.join("DONE").exists());
    }

    #[test]
    fn an_applied_directory_is_removed() {
        let (tmp, store) = open_store();
        let scans = tmp.path().join("scans");
        let scan_dir = running_scan_dir(&scans, "LEFT");
        scan_dir.set_state(State::Completed).unwrap();
        scan_dir.mark_applied().unwrap();
        assert_eq!(
            orphan_ids(&scans),
            [("LEFT".to_string(), OrphanReason::Applied)]
        );
        let orphan = find_orphans(&scans).unwrap().pop().unwrap();
        assert_eq!(
            recover(&store, orphan, "test-version").unwrap(),
            Recovered::Removed
        );
        assert!(!scans.join("LEFT").exists());
    }
}
