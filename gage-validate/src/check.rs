//! The checks one native session file is put through.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::Int64Array;
use datafusion::prelude::SessionContext;
use gage_registry::driver::DriverRegistry;
use gage_session::{ContentSink, ContentSource, Derivation, Driver, Source, SourceOptions};
use gage_store::{SessionStore, Store};
use tokio::runtime::Runtime;

use crate::Error;
use crate::harness::{Harness, harness_version, session_id};

/// What a session is checked for, in the order the checks run. A
/// session stops at its first failed check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Check {
    /// The driver finds the session in the harness layout and opens it
    Discover,
    /// The store takes the session
    Store,
    /// A file of the stored session holds the native file's bytes
    StoredBytes,
    /// The stored session writes back the bytes the store holds
    Restore,
    /// Each entry encodes to the native line it was decoded from
    Codec,
    /// The stored session yields its entry and tool use rows
    Rows,
    /// The source's `entry` table has the stored session's row count
    NativeRows,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Check::Discover => "discover",
            Check::Store => "store",
            Check::StoredBytes => "stored bytes",
            Check::Restore => "restore",
            Check::Codec => "codec",
            Check::Rows => "rows",
            Check::NativeRows => "native rows",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub check: Check,
    pub message: String,
}

/// The outcome of one session file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionResult {
    /// The file's path in its source
    pub file: PathBuf,
    /// The content format the driver stored the session in, when the
    /// session reached the store
    pub content_format: Option<String>,
    /// The harness version the file records, when it records one
    pub harness_version: Option<String>,
    pub failure: Option<Failure>,
}

/// Check the driver of `harness` against `files` of the source `name`
/// checked out at `checkout`. `work` is an empty directory the run
/// keeps the staged sessions, the store, and the driver's cache in.
pub fn check_source(
    name: &str,
    harness: &dyn Harness,
    checkout: &Path,
    files: &[PathBuf],
    work: &Path,
) -> Result<Vec<SessionResult>, Error> {
    let root = work.join("native");
    let mut staged = Vec::new();
    for file in files {
        let id = session_id(name, file);
        let path = harness.stage(&root, &id, &checkout.join(file))?;
        staged.push((file, id, path));
    }

    let driver = DriverRegistry::builtin()
        .for_name(harness.driver())
        .ok_or_else(|| Error::Other(format!("no driver named {:?}", harness.driver())))?;
    let url = format!("{}:{}", harness.driver(), root.display());
    let source = driver
        .open_source(
            SourceOptions::new(&url)
                .show_agents(true)
                .cache_root(work.join("cache")),
        )
        .map_err(other)?;
    let store_path = work.join("store.git");
    gage_store::init(&store_path).map_err(other)?;
    let store = Store::open_with_cache_root(&store_path, &work.join("cache")).map_err(other)?;
    let native = NativeEntries::open(source.as_ref())?;

    let subject = Subject {
        driver,
        source,
        sessions: SessionStore::from(&store),
        native,
    };
    let mut results = Vec::new();
    for (file, id, path) in staged {
        let content = fs::read(&path)?;
        let mut result = SessionResult {
            file: file.clone(),
            content_format: None,
            harness_version: harness_version(harness, &content),
            failure: None,
        };
        result.failure = subject.check(&id, &content, &mut result).err();
        results.push(result);
    }
    Ok(results)
}

struct Subject<'a> {
    driver: Arc<dyn Driver>,
    source: Box<dyn Source>,
    sessions: SessionStore<'a>,
    native: NativeEntries,
}

impl Subject<'_> {
    fn check(&self, id: &str, content: &[u8], result: &mut SessionResult) -> Result<(), Failure> {
        self.source
            .find_native(id)
            .map_err(failed(Check::Discover))?;
        let mut native = self
            .source
            .open_native(id)
            .map_err(failed(Check::Discover))?;

        let added = self
            .sessions
            .add(self.driver.as_ref(), native.as_mut())
            .map_err(failed(Check::Store))?;
        let record = self
            .sessions
            .at_commit(&added.commit_sha)
            .map_err(failed(Check::Store))?;
        let format = record.attrs.content_format;
        result.content_format = Some(format.clone());

        let held = read_all(self.sessions.content(&added.commit_sha).as_ref())
            .map_err(failed(Check::StoredBytes))?;
        if !held.values().any(|bytes| bytes == content) {
            let sizes: Vec<String> = held
                .iter()
                .map(|(path, bytes)| format!("{path} of {} bytes", bytes.len()))
                .collect();
            return Err(Failure {
                check: Check::StoredBytes,
                message: format!(
                    "no stored file holds the {} native bytes; stored: {}",
                    content.len(),
                    sizes.join(", ")
                ),
            });
        }

        let mut stored = self
            .driver
            .read_stored(
                id.to_string(),
                &format,
                self.sessions.content(&added.commit_sha),
            )
            .map_err(failed(Check::Restore))?;
        let mut written = MemorySink::default();
        stored
            .write_derived(&Derivation::default(), &mut written)
            .map_err(failed(Check::Restore))?;
        if written.files != held {
            return Err(Failure {
                check: Check::Restore,
                message: "the stored session writes back bytes the store does not hold".into(),
            });
        }

        let codec = self.driver.codec(&format).map_err(failed(Check::Codec))?;
        let lines: Vec<&[u8]> = content.split(|b| *b == b'\n').collect();
        for entry in stored.entry_values() {
            let entry = entry.map_err(failed(Check::Codec))?;
            let encoded = codec.encode(&entry.value).map_err(failed(Check::Codec))?;
            let line = usize::try_from(entry.line)
                .ok()
                .and_then(|line| line.checked_sub(1))
                .and_then(|index| lines.get(index));
            if line != Some(&encoded.as_slice()) {
                return Err(Failure {
                    check: Check::Codec,
                    message: format!("line {} encodes to other bytes", entry.line),
                });
            }
        }

        let mut entries = 0;
        for entry in stored.entries() {
            entry.map_err(failed(Check::Rows))?;
            entries += 1;
        }
        for tool_use in stored.tool_uses() {
            tool_use.map_err(failed(Check::Rows))?;
        }

        let native = self.native.count(id).map_err(failed(Check::NativeRows))?;
        if native != entries {
            return Err(Failure {
                check: Check::NativeRows,
                message: format!(
                    "the source's entry table has {native} rows; the stored session has {entries}"
                ),
            });
        }
        Ok(())
    }
}

fn failed<E: fmt::Display>(check: Check) -> impl Fn(E) -> Failure {
    move |e| Failure {
        check,
        message: e.to_string(),
    }
}

fn other<E: fmt::Display>(e: E) -> Error {
    Error::Other(e.to_string())
}

fn read_all(content: &dyn ContentSource) -> io::Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for path in content.paths()? {
        let mut bytes = Vec::new();
        content.open(&path)?.read_to_end(&mut bytes)?;
        files.insert(path, bytes);
    }
    Ok(files)
}

#[derive(Default)]
struct MemorySink {
    files: BTreeMap<String, Vec<u8>>,
}

impl ContentSink for MemorySink {
    fn create<'a>(&'a mut self, path: &str) -> io::Result<Box<dyn Write + 'a>> {
        let file = self.files.entry(path.to_string()).or_default();
        file.clear();
        Ok(Box::new(file))
    }
}

/// The `entry` table of a source, queried for a session's row count.
struct NativeEntries {
    runtime: Runtime,
    context: SessionContext,
}

impl NativeEntries {
    fn open(source: &dyn Source) -> Result<Self, Error> {
        let context = SessionContext::new();
        context
            .register_table("entry", source.tables().map_err(other)?.entry)
            .map_err(other)?;
        Ok(Self {
            runtime: Runtime::new()?,
            context,
        })
    }

    fn count(&self, id: &str) -> Result<i64, Error> {
        let sql = format!("SELECT count(*) FROM entry WHERE session_id = '{id}'");
        let batches = self
            .runtime
            .block_on(async { self.context.sql(&sql).await?.collect().await })
            .map_err(other)?;
        batches
            .first()
            .and_then(|batch| batch.column(0).as_any().downcast_ref::<Int64Array>())
            .filter(|counts| !counts.is_empty())
            .map(|counts| counts.value(0))
            .ok_or_else(|| Error::Other("the entry count query returned no count".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::harness;

    const CLAUDE: &str = concat!(
        r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/w/proj","version":"2.0.0","message":{"role":"user","content":"hello there"}}"#,
        "\n",
        r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
        "\n",
        r#"{"type":"user","uuid":"u2","timestamp":"2025-01-01T00:00:03Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"out"}]}}"#,
        "\n",
    );

    fn check(harness_name: &str, file: &str, content: &str) -> SessionResult {
        let checkout = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        fs::write(checkout.path().join(file), content).unwrap();
        let mut results = check_source(
            "test",
            harness(harness_name).unwrap(),
            checkout.path(),
            &[PathBuf::from(file)],
            work.path(),
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        results.remove(0)
    }

    #[test]
    fn a_claude_session_passes_every_check() {
        let result = check("claude", "any-name.jsonl", CLAUDE);
        assert_eq!(result.failure, None);
        assert_eq!(result.harness_version.as_deref(), Some("2.0.0"));
        assert!(result.content_format.is_some());
    }

    #[test]
    fn a_line_the_codec_writes_differently_fails_the_codec_check() {
        let spaced = CLAUDE.replacen(r#"{"type":"user","#, r#"{"type": "user","#, 1);
        let failure = check("claude", "spaced.jsonl", &spaced).failure.unwrap();
        assert_eq!(failure.check, Check::Codec, "{}", failure.message);
        assert_eq!(failure.message, "line 1 encodes to other bytes");
    }
}
