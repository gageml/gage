//! The scan directory of a active scan.
//!
//! A scan is a transaction: while it runs, everything it records goes
//! to `scans/<scan_id>/` under Gage home, and nothing is written to
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
//! scan/plan.json                         # the resolved plan, written at create; see `plan`
//! scan/notes.link                        # the notes' commits, written at apply
//! scan/notes_carried.link                # carried notes' commits, written at apply
//! scan/issues.link                       # the issues' commits, written at apply
//! scan/watermarks/<oid>/<key>            # watermarks, written by watermark and at apply
//! notes/<id>/**                          # note trees, written by write_note
//! issues/<id>/**                         # issue trees, written by write_issue
//! carried_notes                          # carried note commits, appended by carry-forward
//! note_watermarks                        # `<id> <key> <mark>` per watermark on a note the scan wrote, resolved at apply
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
use gage_runtime2::source::SourceFile;
use gage_runtime2::{Level, ScanDirPaths};
use gage_store::{ScanAttrs, TaskAttrs, TaskStatus};
use serde::Serialize;

const STATE_FILE: &str = "state";
const PID_FILE: &str = "pid";
const APPLIED_FILE: &str = "applied";
const SCAN_DIR: &str = "scan";
const NOTES_DIR: &str = "notes";
const NOTES_LINK: &str = "notes.link";
const NOTES_CARRIED_LINK: &str = "notes_carried.link";
const ISSUES_DIR: &str = "issues";
const ISSUES_LINK: &str = "issues.link";
const WATERMARKS_DIR: &str = "watermarks";
const CARRIED_NOTES_FILE: &str = "carried_notes";
const NOTE_WATERMARKS_FILE: &str = "note_watermarks";
const SCANNERS_DIR: &str = "scanners";
const SOURCE_DIR: &str = "sourcecode.d";
const TASKS_DIR: &str = "tasks";
const ATTRS_FILE: &str = "attrs.json";
const DATASET_LINK: &str = "dataset.link";
const PLAN_FILE: &str = "plan.json";
const LOGS_DIR: &str = "logs";
pub(crate) const OUT_LOG: &str = "out";
pub(crate) const ERR_LOG: &str = "err";
pub(crate) const RECORDS_LOG: &str = "records";

/// The parent of every scan directory under Gage home
pub fn scans_dir() -> PathBuf {
    gage_core::config::gage_home().join("scans")
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

/// One active scan's directory
pub struct ScanDir {
    dir: PathBuf,
}

impl ScanDir {
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
    ) -> io::Result<ScanDir> {
        let dir = root.join(scan_id);
        fs::create_dir_all(dir.join(SCAN_DIR))?;
        let scan_dir = ScanDir { dir };
        write_atomic(
            &scan_dir.dir.join(PID_FILE),
            format!("{}\n", std::process::id()).as_bytes(),
        )?;
        if let Some(sha) = dataset {
            write_atomic(
                &scan_dir.object_dir().join(DATASET_LINK),
                format!("{sha}\n").as_bytes(),
            )?;
        }
        for scanner in scanners {
            scan_dir.copy_sources(scanner.name, scanner.sources)?;
            for task in scanner.tasks {
                scan_dir.write_task(
                    scanner.name,
                    task,
                    &TaskAttrs {
                        status: TaskStatus::Pending,
                        started: None,
                        stopped: None,
                        worked_ms: None,
                        skipped: None,
                    },
                )?;
            }
        }
        scan_dir.set_state(State::Running)?;
        Ok(scan_dir)
    }

    /// Copy a scanner's source files to `scan/scanners/<name>/
    /// sourcecode.d/`, each under its stored name, bytes preserved.
    fn copy_sources(&self, scanner: &str, sources: &[SourceFile]) -> io::Result<()> {
        let dir = self
            .object_dir()
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
    pub fn object_dir(&self) -> PathBuf {
        self.dir.join(SCAN_DIR)
    }

    /// The `notes/` directory `write_note` writes note trees under.
    pub fn notes_dir(&self) -> PathBuf {
        self.dir.join(NOTES_DIR)
    }

    /// The note directories, in id order. Empty when no note was
    /// written.
    pub fn note_dirs(&self) -> io::Result<Vec<PathBuf>> {
        object_dirs(&self.notes_dir())
    }

    /// The `issues/` directory `write_issue` writes issue trees under.
    pub fn issues_dir(&self) -> PathBuf {
        self.dir.join(ISSUES_DIR)
    }

    /// The issue directories, in id order. Empty when no issue was
    /// written.
    pub fn issue_dirs(&self) -> io::Result<Vec<PathBuf>> {
        object_dirs(&self.issues_dir())
    }

    /// Write `scan/issues.link` listing `shas`; nothing when empty.
    pub fn write_issues_link(&self, shas: &[String]) -> io::Result<()> {
        self.write_link(ISSUES_LINK, shas)
    }

    /// Write `scan/notes.link` listing `shas`; nothing when empty.
    pub fn write_notes_link(&self, shas: &[String]) -> io::Result<()> {
        self.write_link(NOTES_LINK, shas)
    }

    /// Write `scan/notes_carried.link` listing `shas`; nothing when
    /// empty.
    pub fn write_notes_carried_link(&self, shas: &[String]) -> io::Result<()> {
        self.write_link(NOTES_CARRIED_LINK, shas)
    }

    fn write_link(&self, name: &str, shas: &[String]) -> io::Result<()> {
        if shas.is_empty() {
            return Ok(());
        }
        let content: String = shas.iter().map(|s| format!("{s}\n")).collect();
        write_atomic(&self.object_dir().join(name), content.as_bytes())
    }

    /// The paths the runtime writes under during the run.
    pub fn runtime_paths(&self) -> ScanDirPaths {
        ScanDirPaths {
            dir: self.dir.clone(),
            notes_dir: self.notes_dir(),
            issues_dir: self.issues_dir(),
            note_watermarks: self.dir.join(NOTE_WATERMARKS_FILE),
            watermarks_dir: self.object_dir().join(WATERMARKS_DIR),
            carried_notes: self.dir.join(CARRIED_NOTES_FILE),
            tasks_dir: self.object_dir().join(TASKS_DIR),
        }
    }

    /// The note commits carry-forward appended, one per line; empty
    /// when none.
    pub fn carried_notes(&self) -> io::Result<Vec<String>> {
        match fs::read_to_string(self.dir.join(CARRIED_NOTES_FILE)) {
            Ok(text) => Ok(text.lines().map(String::from).collect()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// The watermarks on the scan's own notes the runtime deferred, as
    /// `(note id, key, mark)` in file order; empty when none.
    pub fn note_watermarks(&self) -> io::Result<Vec<(String, String, u64)>> {
        let text = match fs::read_to_string(self.dir.join(NOTE_WATERMARKS_FILE)) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let parsed = match fields.as_slice() {
                [id, key, mark] => mark.parse::<u64>().ok().map(|m| (*id, *key, m)),
                _ => None,
            };
            let Some((id, key, mark)) = parsed else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("note watermark line is not <id> <key> <mark>: {line:?}"),
                ));
            };
            out.push((id.to_string(), key.to_string(), mark));
        }
        Ok(out)
    }

    /// Write `scan/watermarks/<oid>/<key>` holding `<commit> <mark>`.
    pub fn write_watermark(&self, oid: &str, key: &str, commit: &str, mark: u64) -> io::Result<()> {
        let dir = self.object_dir().join(WATERMARKS_DIR).join(oid);
        fs::create_dir_all(&dir)?;
        write_atomic(&dir.join(key), format!("{commit} {mark}\n").as_bytes())
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
        Logs::new(scan_logs_dir(&self.object_dir()))
    }

    /// Write `scan/plan.json`, the resolved plan (see `crate::plan`).
    pub fn write_plan(&self, plan: &serde_json::Value) -> io::Result<()> {
        write_json(&self.object_dir().join(PLAN_FILE), plan)
    }

    /// Write the scan's `attrs.json`.
    pub fn write_scan(&self, attrs: &ScanAttrs) -> io::Result<()> {
        write_json(&self.object_dir().join(ATTRS_FILE), attrs)
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
        self.object_dir().join(TASKS_DIR).join(scanner).join(task)
    }
}

/// The subdirectories of `dir`, sorted by name. Empty when `dir` does
/// not exist.
fn object_dirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
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

/// The `logs/` directory of the scan under a scan directory's `scan/` subtree
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
        let scan_dir = ScanDir::create(tmp.path(), "SCAN1", None, &plan).unwrap();
        assert_eq!(scan_dir.dir(), tmp.path().join("SCAN1"));
        assert!(!scan_dir.object_dir().join("dataset.link").exists());
        assert_eq!(
            fs::read_to_string(
                scan_dir
                    .object_dir()
                    .join("scanners/hello/sourcecode.d/hello.rn")
            )
            .unwrap(),
            "pub fn greet() {}\n"
        );
        assert_eq!(
            fs::read_to_string(scan_dir.dir().join("state")).unwrap(),
            "running\n"
        );
        assert_eq!(
            fs::read_to_string(scan_dir.dir().join("pid")).unwrap(),
            format!("{}\n", std::process::id())
        );

        scan_dir
            .write_task(
                "hello",
                "fail",
                &TaskAttrs {
                    status: TaskStatus::Failed,
                    started: Some(1),
                    stopped: Some(2),
                    worked_ms: None,
                    skipped: None,
                },
            )
            .unwrap();
        let mut logs = scan_dir.scan_logs();
        logs.err("boom").unwrap();
        logs.out("hello, ").unwrap();
        logs.out("world\n").unwrap();
        logs.record(Level::Info, "hello:greet", "started").unwrap();
        drop(logs);
        assert_eq!(
            fs::read_to_string(scan_dir.object_dir().join("logs/out")).unwrap(),
            "hello, world\n"
        );
        let records = fs::read_to_string(scan_dir.object_dir().join("logs/records")).unwrap();
        assert!(
            records.ends_with("Z INFO hello:greet: started\n"),
            "{records}"
        );
        assert_eq!(
            fs::read_to_string(scan_dir.object_dir().join("logs/err")).unwrap(),
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
        scan_dir.write_scan(&attrs).unwrap();
        scan_dir.set_state(State::Completed).unwrap();

        let content = ScanContent::from_files(&DirFiles::new(&scan_dir.object_dir())).unwrap();
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
            !scan_dir.object_dir().join("attrs.json.tmp").exists(),
            "temp file is renamed away"
        );

        scan_dir.mark_applied().unwrap();
        let dir = scan_dir.dir().to_path_buf();
        assert!(dir.join("applied").exists());
        scan_dir.remove().unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn create_writes_the_dataset_link() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let scan_dir = ScanDir::create(tmp.path(), "SCAN2", Some(sha), &[]).unwrap();
        assert_eq!(
            fs::read_to_string(scan_dir.object_dir().join("dataset.link")).unwrap(),
            format!("{sha}\n")
        );
        let content = ScanContent::from_files(&DirFiles::new(&scan_dir.object_dir()));
        // attrs.json is absent until the terminal state, so the decoder
        // stops there; the link itself is what this test checks
        assert!(content.is_err());
    }
}
