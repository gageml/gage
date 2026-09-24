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
//! scan/dataset.link                      # the scanned dataset's commit, written at create
//! scan/notes.link                        # the notes' commits, written at apply
//! notes/<id>/**                          # note trees, written by write_note
//! scan/logs/out                          # output lines, the scan's own and every task's
//! scan/logs/err                          # error lines: task failures, the cancel notice, panics
//! scan/logs/records                      # log records, runtime and scanner
//! scan/scanners/<name>/sourcecode.d/<f>  # scanner source as run, copied at create
//! scan/tasks/<scanner>/<task>/attrs.json # pending at create, rewritten on start and finish
//! ```
//!
//! Every file except the logs is written whole through a temp file
//! and a rename, so a crash never leaves a torn file. The logs are
//! streams, appended through an open handle; a crash leaves a valid
//! prefix. A directory left behind by a crash stays in `running` with
//! a dead pid; recovery is not implemented yet.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use gage_core::datetime::{ms_to_iso8601, now_ms};
use gage_runtime2::Level;
use gage_runtime2::source::SourceFile;
use gage_store::{ScanAttrs, TaskAttrs, TaskStatus};
use serde::Serialize;

const STATE_FILE: &str = "state";
const PID_FILE: &str = "pid";
const APPLIED_FILE: &str = "applied";
const SCAN_DIR: &str = "scan";
const NOTES_DIR: &str = "notes";
const NOTES_LINK: &str = "notes.link";
const SCANNERS_DIR: &str = "scanners";
const SOURCE_DIR: &str = "sourcecode.d";
const TASKS_DIR: &str = "tasks";
const ATTRS_FILE: &str = "attrs.json";
const DATASET_LINK: &str = "dataset.link";
const LOGS_DIR: &str = "logs";
pub(crate) const OUT_LOG: &str = "out";
pub(crate) const ERR_LOG: &str = "err";
pub(crate) const RECORDS_LOG: &str = "records";

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

/// One scanner of a planned scan: its tasks and its source files.
pub struct ScannerPlan<'a> {
    pub name: &'a str,
    pub tasks: &'a [String],
    pub sources: &'a [SourceFile],
}

/// One scan's staging directory.
pub struct Staging {
    dir: PathBuf,
}

impl Staging {
    /// Create `root/<scan_id>/` in the `running` state with this
    /// process's pid, `scan/dataset.link` naming `dataset` when the
    /// scan has one, every scanner's source copied under
    /// `scan/scanners/<name>/sourcecode.d/`, and one `pending` task
    /// record per task.
    pub fn create(
        root: &Path,
        scan_id: &str,
        dataset: Option<&str>,
        scanners: &[ScannerPlan],
    ) -> io::Result<Staging> {
        let dir = root.join(scan_id);
        fs::create_dir_all(dir.join(SCAN_DIR))?;
        let staging = Staging { dir };
        write_atomic(
            &staging.dir.join(PID_FILE),
            format!("{}\n", std::process::id()).as_bytes(),
        )?;
        if let Some(sha) = dataset {
            write_atomic(
                &staging.scan_dir().join(DATASET_LINK),
                format!("{sha}\n").as_bytes(),
            )?;
        }
        for scanner in scanners {
            staging.copy_sources(scanner.name, scanner.sources)?;
            for task in scanner.tasks {
                staging.write_task(
                    scanner.name,
                    task,
                    &TaskAttrs {
                        status: TaskStatus::Pending,
                        started: None,
                        stopped: None,
                        worked_ms: None,
                    },
                )?;
            }
        }
        staging.set_state(State::Running)?;
        Ok(staging)
    }

    /// Copy a scanner's source files to `scan/scanners/<name>/
    /// sourcecode.d/`, each under its stored name, bytes preserved.
    fn copy_sources(&self, scanner: &str, sources: &[SourceFile]) -> io::Result<()> {
        let dir = self
            .scan_dir()
            .join(SCANNERS_DIR)
            .join(scanner)
            .join(SOURCE_DIR);
        fs::create_dir_all(&dir)?;
        for source in sources {
            let bytes = fs::read(&source.path)?;
            write_atomic(&dir.join(&source.name), &bytes)?;
        }
        Ok(())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The `scan/` subtree, the content applied to the store.
    pub fn scan_dir(&self) -> PathBuf {
        self.dir.join(SCAN_DIR)
    }

    /// The `notes/` directory `write_note` stages note trees under.
    pub fn notes_dir(&self) -> PathBuf {
        self.dir.join(NOTES_DIR)
    }

    /// The staged note directories, in id order. Empty when no note
    /// was written.
    pub fn staged_notes(&self) -> io::Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(self.notes_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut dirs = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                dirs.push(path);
            }
        }
        dirs.sort();
        Ok(dirs)
    }

    /// Write `scan/notes.link` listing `shas`; nothing when empty.
    pub fn write_notes_link(&self, shas: &[String]) -> io::Result<()> {
        if shas.is_empty() {
            return Ok(());
        }
        let content: String = shas.iter().map(|s| format!("{s}\n")).collect();
        write_atomic(&self.scan_dir().join(NOTES_LINK), content.as_bytes())
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

    /// The log appenders of the scan. Each log is created on its
    /// first write, so a scan that produced nothing has no `logs/`
    /// entry.
    pub fn scan_logs(&self) -> Logs {
        Logs::new(scan_logs_dir(&self.scan_dir()))
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

/// The `logs/` directory of the scan under a staging `scan/` directory
pub(crate) fn scan_logs_dir(scan_dir: &Path) -> PathBuf {
    scan_dir.join(LOGS_DIR)
}

/// Append `bytes` to `dir/name`, creating both as needed. One open
/// per call; for a stream of writes use [`Logs`].
pub(crate) fn append(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    open_append(dir, name)?.write_all(bytes)
}

/// Appenders for one `logs/` directory: `out`, `err`, and `records`.
pub struct Logs {
    dir: PathBuf,
    out: Option<File>,
    err: Option<File>,
    records: Option<File>,
}

impl Logs {
    fn new(dir: PathBuf) -> Self {
        Logs {
            dir,
            out: None,
            err: None,
            records: None,
        }
    }

    /// Append output verbatim.
    pub fn out(&mut self, s: &str) -> io::Result<()> {
        let dir = &self.dir;
        let file = match &mut self.out {
            Some(file) => file,
            None => self.out.insert(open_append(dir, OUT_LOG)?),
        };
        file.write_all(s.as_bytes())
    }

    /// Append error output verbatim, newline-terminated.
    pub fn err(&mut self, s: &str) -> io::Result<()> {
        let dir = &self.dir;
        let file = match &mut self.err {
            Some(file) => file,
            None => self.err.insert(open_append(dir, ERR_LOG)?),
        };
        file.write_all(s.as_bytes())?;
        if !s.ends_with('\n') {
            file.write_all(b"\n")?;
        }
        Ok(())
    }

    /// Append one record: `<ISO 8601 ms UTC> <LEVEL> <origin>:
    /// <message>`. `origin` is `<scanner>:<task>` for a scanner's
    /// record.
    pub fn record(&mut self, level: Level, origin: &str, message: &str) -> io::Result<()> {
        let dir = &self.dir;
        let file = match &mut self.records {
            Some(file) => file,
            None => self.records.insert(open_append(dir, RECORDS_LOG)?),
        };
        let line = format!(
            "{} {} {origin}: {message}\n",
            ms_to_iso8601(now_ms()),
            level.as_str().to_uppercase()
        );
        file.write_all(line.as_bytes())
    }
}

fn open_append(dir: &Path, name: &str) -> io::Result<File> {
    fs::create_dir_all(dir)?;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(name))
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
    use std::collections::BTreeMap;

    use gage_store::{DirFiles, ScanContent, TaskCounts};

    use super::*;

    #[test]
    fn create_records_the_plan_and_writes_round_trip_through_the_store_decoder() {
        let tmp = tempfile::tempdir().unwrap();
        let scanner_file = tmp.path().join("src").join("hello.rn");
        fs::create_dir_all(scanner_file.parent().unwrap()).unwrap();
        fs::write(&scanner_file, "pub fn greet() {}\n").unwrap();
        let sources = gage_runtime2::source::source_files(&scanner_file).unwrap();
        let tasks = vec!["greet".to_string(), "fail".to_string()];
        let plan = [ScannerPlan {
            name: "hello",
            tasks: &tasks,
            sources: &sources,
        }];
        let staging = Staging::create(tmp.path(), "SCAN1", None, &plan).unwrap();
        assert_eq!(staging.dir(), tmp.path().join("SCAN1"));
        assert!(!staging.scan_dir().join("dataset.link").exists());
        assert_eq!(
            fs::read_to_string(
                staging
                    .scan_dir()
                    .join("scanners/hello/sourcecode.d/hello.rn")
            )
            .unwrap(),
            "pub fn greet() {}\n"
        );
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
        let mut logs = staging.scan_logs();
        logs.err("boom").unwrap();
        logs.out("hello, ").unwrap();
        logs.out("world\n").unwrap();
        logs.record(Level::Info, "hello:greet", "started").unwrap();
        drop(logs);
        assert_eq!(
            fs::read_to_string(staging.scan_dir().join("logs/out")).unwrap(),
            "hello, world\n"
        );
        let records = fs::read_to_string(staging.scan_dir().join("logs/records")).unwrap();
        assert!(
            records.ends_with("Z INFO hello:greet: started\n"),
            "{records}"
        );
        assert_eq!(
            fs::read_to_string(staging.scan_dir().join("logs/err")).unwrap(),
            "boom\n",
            "err is newline-terminated"
        );
        let attrs = ScanAttrs {
            runtime: "gage test".into(),
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
        assert_eq!(content.tasks[1].task, "greet");
        assert_eq!(content.tasks[1].attrs.status, TaskStatus::Pending);
        assert_eq!(content.logs, ["err", "out", "records"]);
        assert_eq!(
            content.scanners,
            BTreeMap::from([("hello".to_string(), vec!["hello.rn".to_string()])])
        );
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

    #[test]
    fn create_writes_the_dataset_link() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let staging = Staging::create(tmp.path(), "SCAN2", Some(sha), &[]).unwrap();
        assert_eq!(
            fs::read_to_string(staging.scan_dir().join("dataset.link")).unwrap(),
            format!("{sha}\n")
        );
        let content = ScanContent::from_files(&DirFiles::new(&staging.scan_dir()));
        // attrs.json is absent until the terminal state, so the decoder
        // stops there; the link itself is what this test checks
        assert!(content.is_err());
    }
}
