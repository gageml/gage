//! Session files: how one is named, what its header says about the
//! session, and where they sit under a sessions directory.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use regex::Regex;
use serde_json::Value;

/// The entry type of the header, the first line of a session file
pub(crate) const HEADER_TYPE: &str = "session";

/// The newest version of Pi's session format this build reads
pub(crate) const MAX_FORMAT_VERSION: u32 = 3;

static FILE_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-\d{3}Z_([A-Za-z0-9._-]+)\.jsonl$").unwrap()
});

/// The session id a session file name carries, the part after the
/// creation time; `None` for a name that is not a session file's. Pi
/// lets a user choose the id, from letters, digits, `.`, `_`, and `-`.
pub(crate) fn session_id(file_name: &str) -> Option<&str> {
    Some(FILE_NAME_RE.captures(file_name)?.get(1)?.as_str())
}

/// What a session file's header says about the session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Header {
    /// The version of Pi's session format the file is in. A header
    /// from before the format was versioned names none and is version
    /// 1.
    pub version: u32,
    /// The working directory the session started in
    pub cwd: Option<PathBuf>,
}

impl Header {
    /// The header `line` is; `None` when the line is not one. A
    /// header is a `session` entry that names the session's id.
    pub(crate) fn from_line(line: &Value) -> Option<Self> {
        if line.get("type").and_then(Value::as_str) != Some(HEADER_TYPE) {
            return None;
        }
        line.get("id").and_then(Value::as_str)?;
        let version = match line.get("version").and_then(Value::as_u64) {
            Some(version) => u32::try_from(version).unwrap_or(u32::MAX),
            None => 1,
        };
        Some(Self {
            version,
            cwd: line
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
                .map(PathBuf::from),
        })
    }
}

/// The header of each session file read so far, by path. A header
/// keeps its working directory for as long as the file exists, so one
/// read serves every later listing. Pi raises the header's version
/// when it migrates a file, so the version is read from the file
/// wherever it matters and never from here.
static HEADERS: LazyLock<Mutex<HashMap<PathBuf, Header>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The header of the session file at `path`: the first line of it
/// that parses, when that line is a header. Pi passes over blank
/// lines and lines that do not parse the same way. `None` is a file
/// that is not a Pi session, and is not remembered: Pi writes a header
/// into an empty file it is asked to open.
pub(crate) fn read_header(path: &Path) -> io::Result<Option<Header>> {
    if let Some(header) = HEADERS.lock().expect("header cache lock").get(path) {
        return Ok(Some(header.clone()));
    }
    let Some(header) = first_parsed_line(path)?.and_then(|line| Header::from_line(&line)) else {
        return Ok(None);
    };
    HEADERS
        .lock()
        .expect("header cache lock")
        .insert(path.to_path_buf(), header.clone());
    Ok(Some(header))
}

fn first_parsed_line(path: &Path) -> io::Result<Option<Value>> {
    for line in BufReader::new(File::open(path)?).split(b'\n') {
        // A line that is not JSON, which includes one that is not
        // UTF-8, is one Pi skips.
        if let Ok(value) = serde_json::from_slice::<Value>(&line?) {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

/// One session file of a sessions directory
pub(crate) struct SessionFile {
    pub id: String,
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub size: u64,
}

/// Every file under `sessions_dir` named as a session file, in no
/// order. Pi files a session under a directory named for its working
/// directory, and flat in the sessions directory when the user moved
/// it, so the walk reads the directory and one level below it. A
/// missing directory holds no sessions.
pub(crate) fn session_files(sessions_dir: &Path) -> io::Result<Vec<SessionFile>> {
    let mut out = Vec::new();
    collect(sessions_dir, 1, &mut out)?;
    Ok(out)
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<SessionFile>) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        // A file removed while the directory is read is not a session
        // any more.
        let meta = match path.metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if meta.is_dir() {
            if depth > 0 {
                collect(&path, depth - 1, out)?;
            }
            continue;
        }
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(session_id)
        else {
            continue;
        };
        out.push(SessionFile {
            id: id.to_string(),
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: meta.len(),
            path,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    #[test]
    fn a_session_file_name_carries_the_id_after_the_creation_time() {
        assert_eq!(
            session_id("2026-11-20T12-00-00-500Z_01a1f8d3-eafa-761f-b8f0-d10eaf4e7c6d.jsonl"),
            Some("01a1f8d3-eafa-761f-b8f0-d10eaf4e7c6d")
        );
        assert_eq!(
            session_id("2026-11-20T12-00-00-500Z_my_run.v2.jsonl"),
            Some("my_run.v2"),
            "an id a user chose, with an underscore of its own"
        );
        assert_eq!(
            session_id("01a1f8d3-eafa-761f-b8f0-d10eaf4e7c6d.jsonl"),
            None
        );
        assert_eq!(session_id("2026-11-20T12-00-00-500Z_.jsonl"), None);
        assert_eq!(session_id("2026-11-20T12-00-00-500Z_id.jsonl.bak"), None);
    }

    #[test]
    fn a_header_names_the_version_and_the_working_directory() {
        let header = |line: Value| Header::from_line(&line);
        assert_eq!(
            header(json!({"type": "session", "version": 3, "id": "s", "cwd": "/w/proj"})),
            Some(Header {
                version: 3,
                cwd: Some(PathBuf::from("/w/proj")),
            })
        );
        assert_eq!(
            header(json!({"type": "session", "id": "s", "cwd": ""})),
            Some(Header {
                version: 1,
                cwd: None,
            }),
            "a header from before the format was versioned"
        );
        assert_eq!(header(json!({"type": "session", "version": 3})), None);
        assert_eq!(header(json!({"type": "message", "id": "a1"})), None);
    }

    #[test]
    fn the_header_is_the_first_line_that_parses() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: &str| {
            let path = dir.path().join(name);
            fs::write(&path, text).unwrap();
            path
        };
        let header = r#"{"type":"session","version":3,"id":"s","cwd":"/w/proj"}"#;
        let message = r#"{"type":"message","id":"a1","parentId":null}"#;

        let led = write("led.jsonl", &format!("\nnot json\n{header}\n{message}\n"));
        assert_eq!(read_header(&led).unwrap().unwrap().version, 3);

        let late = write("late.jsonl", &format!("{message}\n{header}\n"));
        assert_eq!(
            read_header(&late).unwrap(),
            None,
            "a file Pi itself does not open as a session"
        );

        let empty = write("empty.jsonl", "");
        assert_eq!(read_header(&empty).unwrap(), None);
        fs::write(&empty, format!("{header}\n")).unwrap();
        assert!(
            read_header(&empty).unwrap().is_some(),
            "a file with no header is read again"
        );
    }

    #[test]
    fn the_walk_reads_the_sessions_directory_and_one_level_below() {
        let dir = tempfile::tempdir().unwrap();
        let name = |id: &str| format!("2026-01-02T03-04-05-006Z_{id}.jsonl");
        let project = dir.path().join("--w-proj--");
        fs::create_dir_all(project.join("deeper")).unwrap();
        fs::write(dir.path().join(name("flat")), "").unwrap();
        fs::write(project.join(name("filed")), "").unwrap();
        fs::write(project.join("notes.jsonl"), "").unwrap();
        fs::write(project.join("deeper").join(name("deep")), "").unwrap();

        let mut ids: Vec<String> = session_files(dir.path())
            .unwrap()
            .into_iter()
            .map(|file| file.id)
            .collect();
        ids.sort();
        assert_eq!(ids, ["filed", "flat"]);
        assert!(
            session_files(&dir.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }
}
