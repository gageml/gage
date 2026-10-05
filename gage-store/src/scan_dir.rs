//! The layout of a scan directory: the on-disk state of an active
//! scan under `scans/<scan_id>/` in Gage home.
//!
//! ```text
//! state                        # running | completed | canceled
//! pid                          # writer process, present while running
//! applied                      # present once written to the store
//! scan/**                      # the scan object's tree as it will be committed
//! scan/dataset.link            # the scanned dataset's commit, written at create
//! notes/<id>/**                # note trees the scan wrote
//! issues/<id>/**               # issue trees the scan wrote
//! carried_notes                # carried note commits, one per line
//! note_watermarks              # `<id> <key> <mark>` per watermark on a note the scan wrote
//! ```
//!
//! The orchestrator writes the directory and the query layer reads
//! it, so both take the paths from here.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const STATE_FILE: &str = "state";
pub const PID_FILE: &str = "pid";
pub const APPLIED_FILE: &str = "applied";
/// The scan object's tree
pub const OBJECT_DIR: &str = "scan";
pub const NOTES_DIR: &str = "notes";
pub const ISSUES_DIR: &str = "issues";
pub const CARRIED_NOTES_FILE: &str = "carried_notes";
pub const NOTE_WATERMARKS_FILE: &str = "note_watermarks";
/// Under the object tree
pub const WATERMARKS_DIR: &str = "watermarks";
pub const TASKS_DIR: &str = "tasks";
pub const DATASET_LINK: &str = "dataset.link";
pub const PLAN_FILE: &str = "plan.json";

/// The paths of one scan directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanDirLayout {
    root: PathBuf,
}

impl ScanDirLayout {
    /// The layout rooted at `root`, `scans/<scan_id>/`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The scan's id: the directory name.
    pub fn scan_id(&self) -> Option<&str> {
        self.root.file_name().and_then(|n| n.to_str())
    }

    /// The scan object's tree, `scan/`.
    pub fn object_dir(&self) -> PathBuf {
        self.root.join(OBJECT_DIR)
    }

    pub fn notes_dir(&self) -> PathBuf {
        self.root.join(NOTES_DIR)
    }

    pub fn issues_dir(&self) -> PathBuf {
        self.root.join(ISSUES_DIR)
    }

    pub fn carried_notes(&self) -> PathBuf {
        self.root.join(CARRIED_NOTES_FILE)
    }

    pub fn note_watermarks(&self) -> PathBuf {
        self.root.join(NOTE_WATERMARKS_FILE)
    }

    pub fn watermarks_dir(&self) -> PathBuf {
        self.object_dir().join(WATERMARKS_DIR)
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.object_dir().join(TASKS_DIR)
    }

    pub fn dataset_link(&self) -> PathBuf {
        self.object_dir().join(DATASET_LINK)
    }

    pub fn plan_file(&self) -> PathBuf {
        self.object_dir().join(PLAN_FILE)
    }

    /// The note directories, sorted by id. Empty when no note was
    /// written.
    pub fn note_dirs(&self) -> io::Result<Vec<PathBuf>> {
        subdirs(&self.notes_dir())
    }

    /// The issue directories, sorted by id. Empty when no issue was
    /// written.
    pub fn issue_dirs(&self) -> io::Result<Vec<PathBuf>> {
        subdirs(&self.issues_dir())
    }

    /// The carried note commits, one per line, in the order they were
    /// carried; empty when none.
    pub fn carried_note_commits(&self) -> io::Result<Vec<String>> {
        match fs::read_to_string(self.carried_notes()) {
            Ok(text) => Ok(text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// The dataset commit from `scan/dataset.link`, or `None` when the
    /// scan has no dataset.
    pub fn dataset_commit(&self) -> io::Result<Option<String>> {
        match fs::read_to_string(self.dataset_link()) {
            Ok(text) => Ok(Some(text.trim().to_string()).filter(|s| !s.is_empty())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// The subdirectories of `dir`, sorted by name. Empty when `dir` does
/// not exist.
fn subdirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
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
