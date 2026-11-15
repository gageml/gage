//! Rollout files: how one is named, what its first record says about
//! the session, and the thread names Codex keeps beside them.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use regex::Regex;
use serde_json::Value;

/// The directory of rollout files under a Codex home
pub(crate) const SESSIONS_DIR: &str = "sessions";

/// The file of thread names under a Codex home: one JSON object per
/// line, `{id, thread_name, updated_at}`, appended on every rename
const SESSION_INDEX_FILE: &str = "session_index.jsonl";

/// The `thread_source` of a thread a person started
const USER_THREAD: &str = "user";

static ROLLOUT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^rollout-\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$",
    )
    .unwrap()
});

/// The thread id a rollout file name carries; `None` for a name that
/// is not a rollout's.
pub(crate) fn rollout_id(file_name: &str) -> Option<&str> {
    Some(ROLLOUT_RE.captures(file_name)?.get(1)?.as_str())
}

/// What a rollout's first record, `session_meta`, says about the
/// session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionMeta {
    /// The working directory the session started in
    pub cwd: Option<PathBuf>,
    /// What started the thread: `user`, or the name of whatever else
    /// did, such as a sub-agent spawn or Gage's own scan agents
    pub thread_source: Option<String>,
}

impl SessionMeta {
    /// The meta of a rollout's first line; the default when the line
    /// is not a `session_meta` record.
    pub(crate) fn from_line(line: &Value) -> Self {
        if line.get("type").and_then(Value::as_str) != Some("session_meta") {
            return Self::default();
        }
        let field = |name| {
            line.pointer(&format!("/payload/{name}"))
                .and_then(Value::as_str)
                .map(String::from)
        };
        Self {
            cwd: field("cwd").map(PathBuf::from),
            thread_source: field("thread_source"),
        }
    }

    /// Whether a person started the thread. A record that names no
    /// source is a person's: only a harness that starts threads for
    /// other reasons names them.
    pub(crate) fn is_user_thread(&self) -> bool {
        self.thread_source
            .as_deref()
            .is_none_or(|source| source == USER_THREAD)
    }
}

/// The meta of each rollout read so far, by path. A rollout's first
/// line is written once, so one read serves every later listing.
static METAS: LazyLock<Mutex<HashMap<PathBuf, SessionMeta>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The meta of the rollout at `path`, read from its first line. A
/// first line that is not JSON reads as the default meta: the listing
/// still names the session, and deriving it reports what is wrong.
pub(crate) fn read_meta(path: &Path) -> io::Result<SessionMeta> {
    if let Some(meta) = METAS.lock().expect("meta cache lock").get(path) {
        return Ok(meta.clone());
    }
    let mut first = String::new();
    BufReader::new(File::open(path)?).read_line(&mut first)?;
    let meta = match serde_json::from_str::<Value>(&first) {
        Ok(line) => SessionMeta::from_line(&line),
        Err(_) => SessionMeta::default(),
    };
    METAS
        .lock()
        .expect("meta cache lock")
        .insert(path.to_path_buf(), meta.clone());
    Ok(meta)
}

/// One rollout file of a Codex home
pub(crate) struct RolloutFile {
    pub id: String,
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub size: u64,
}

/// Every rollout file under `sessions_dir`, in no order. Rollouts sit
/// three directories down, by year, month, and day; a file that is
/// not named as a rollout is passed over. A missing directory holds
/// no rollouts.
pub(crate) fn rollout_files(sessions_dir: &Path) -> io::Result<Vec<RolloutFile>> {
    let mut out = Vec::new();
    collect(sessions_dir, 3, &mut out)?;
    Ok(out)
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<RolloutFile>) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        // A file removed while the directory is read is not a rollout
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
            .and_then(rollout_id)
        else {
            continue;
        };
        out.push(RolloutFile {
            id: id.to_string(),
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: meta.len(),
            path,
        });
    }
    Ok(())
}

/// The thread names Codex records in the Codex home `home`, by thread
/// id, read once per change of the session index: a listing asks for
/// the name of every session it shows.
#[derive(Debug, Default)]
pub(crate) struct ThreadNames {
    read: Mutex<Option<ReadNames>>,
}

/// The names as last read, with the index file's state at that read
#[derive(Debug)]
struct ReadNames {
    modified: Option<SystemTime>,
    size: u64,
    names: HashMap<String, String>,
}

impl ThreadNames {
    /// The name recorded for the thread `id`: the last entry of the
    /// session index that names the thread. `None` when the index is
    /// missing or does not name the thread; a session without a
    /// recorded name keeps the title derived from its content. An
    /// index that cannot be read is logged and names nothing.
    pub(crate) fn name(&self, home: &Path, id: &str) -> Option<String> {
        let path = home.join(SESSION_INDEX_FILE);
        let (modified, size) = match path.metadata() {
            Ok(meta) => (meta.modified().ok(), meta.len()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(path = %path.display(), "thread names not read: {e}");
                return None;
            }
        };
        let mut read = self.read.lock().expect("thread names lock");
        let current = read
            .as_ref()
            .is_some_and(|names| names.modified == modified && names.size == size);
        if !current {
            let names = match read_thread_names(&path) {
                Ok(names) => names,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "thread names not read: {e}");
                    HashMap::new()
                }
            };
            *read = Some(ReadNames {
                modified,
                size,
                names,
            });
        }
        read.as_ref()?.names.get(id).cloned()
    }
}

/// Every thread's name in the session index at `path`: for each
/// thread, the last entry that names it. A name that is blank names
/// nothing.
fn read_thread_names(path: &Path) -> io::Result<HashMap<String, String>> {
    let mut names = HashMap::new();
    for line in BufReader::new(File::open(path)?).lines() {
        // A line being appended while the index is read does not
        // parse; the names before it stand.
        let Ok(entry) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        let field = |key| entry.get(key).and_then(Value::as_str);
        if let (Some(id), Some(name)) = (field("id"), field("thread_name")) {
            match name.trim().is_empty() {
                true => names.remove(id),
                false => names.insert(id.to_string(), name.to_string()),
            };
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_rollout_file_name_carries_the_thread_id() {
        assert_eq!(
            rollout_id("rollout-2026-01-02T03-04-05-01a1dfb5-c0a3-7690-90ea-73e7ddf93b90.jsonl"),
            Some("01a1dfb5-c0a3-7690-90ea-73e7ddf93b90")
        );
        assert_eq!(
            rollout_id("01a1dfb5-c0a3-7690-90ea-73e7ddf93b90.jsonl"),
            None
        );
        assert_eq!(
            rollout_id(
                "rollout-2026-01-02T03-04-05-01a1dfb5-c0a3-7690-90ea-73e7ddf93b90.jsonl.zst"
            ),
            None
        );
    }

    #[test]
    fn a_thread_is_a_persons_unless_its_meta_names_another_source() {
        let meta = |source: Option<&str>| {
            SessionMeta::from_line(&json!({
                "type": "session_meta",
                "payload": {"cwd": "/home/u/proj", "thread_source": source, "history_mode": "paginated"},
            }))
        };
        assert!(meta(Some("user")).is_user_thread());
        assert!(meta(None).is_user_thread());
        assert!(!meta(Some("gage-scan")).is_user_thread());
        assert_eq!(meta(None).cwd, Some(PathBuf::from("/home/u/proj")));
        assert_eq!(
            SessionMeta::from_line(&json!({"type": "turn_context"})),
            SessionMeta::default()
        );
    }

    #[test]
    fn the_last_index_entry_for_a_thread_names_it() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(SESSION_INDEX_FILE),
            concat!(
                r#"{"id":"t1","thread_name":"First","updated_at":"2026-01-01T00:00:00Z"}"#,
                "\n",
                r#"{"id":"t2","thread_name":"Other","updated_at":"2026-01-01T00:00:01Z"}"#,
                "\n",
                r#"{"id":"t1","thread_name":"Renamed","updated_at":"2026-01-01T00:00:02Z"}"#,
                "\n",
            ),
        )
        .unwrap();
        let names = ThreadNames::default();
        assert_eq!(names.name(home.path(), "t1").as_deref(), Some("Renamed"));
        assert_eq!(names.name(home.path(), "t3"), None);
        assert_eq!(names.name(&home.path().join("missing"), "t1"), None);
    }
}
