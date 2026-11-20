//! The checks one native session file is put through.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str;
use std::sync::Arc;

use arrow::array::Int64Array;
use datafusion::prelude::SessionContext;
use gage_registry::driver::DriverRegistry;
use gage_session::{
    ContentSink, ContentSource, Derivation, Driver, EntryValue, Source, SourceOptions,
};
use gage_store::{SessionStore, Store};
use serde_json::Value;
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
    /// Each line reads to the value `serde_json` reads, or neither
    /// reads it
    ReaderAgreement,
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
            Check::ReaderAgreement => "reader agreement",
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

        reader_agreement(content).map_err(failed(Check::ReaderAgreement))?;

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

/// Read each line of `content` with the entry reader and with
/// `serde_json`, and name the first line the two read differently. A
/// blank line is no entry and is skipped, as a session reader skips
/// it. A line neither reads, a torn write among them, is agreement:
/// what becomes of it is the codec's to decide.
fn reader_agreement(content: &[u8]) -> Result<(), String> {
    for (index, line) in content.split(|b| *b == b'\n').enumerate() {
        if line.trim_ascii().is_empty() {
            continue;
        }
        // Why a reader refuses a line is dropped: the check compares
        // whether each reads it, and what it reads.
        let entry = str::from_utf8(line)
            .ok()
            .and_then(|text| text.parse::<EntryValue>().ok());
        let json = serde_json::from_slice::<Value>(line).ok();
        same_reading(entry.as_ref(), json.as_ref())
            .map_err(|difference| format!("line {}: {difference}", index + 1))?;
    }
    Ok(())
}

/// Whether the entry reader and `serde_json` read one line alike:
/// both refuse it, or both read it to the same value. The entry is
/// compared as the `serde_json::Value` it converts to, in which a
/// number is its value and an object has no key order. A number
/// written `1.50` or `1E3` is therefore equal to the one `serde_json`
/// read; that its text is kept is for the codec check to show.
fn same_reading(entry: Option<&EntryValue>, json: Option<&Value>) -> Result<(), &'static str> {
    match (entry, json) {
        (None, None) => Ok(()),
        (Some(_), None) => Err("the entry reader reads it and serde_json does not"),
        (None, Some(_)) => Err("serde_json reads it and the entry reader does not"),
        (Some(entry), Some(json)) if entry.to_json_value() == *json => Ok(()),
        (Some(_), Some(_)) => Err("the entry reader reads another value than serde_json"),
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
    fn a_pi_session_passes_every_check() {
        let session = concat!(
            r#"{"type":"session","version":3,"id":"s","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/w/proj"}"#,
            "\n",
            r#"{"type":"message","id":"a0000001","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"hello there","timestamp":1}}"#,
            "\n",
            r#"{"type":"message","id":"a0000002","parentId":"a0000001","timestamp":"2025-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"model":"m","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.000005}},"stopReason":"stop","timestamp":2}}"#,
            "\n",
        );
        let result = check("pi", "any-name.jsonl", session);
        assert_eq!(result.failure, None);
        assert_eq!(result.harness_version.as_deref(), Some("format-v3"));
        assert_eq!(result.content_format.as_deref(), Some("pi-jsonl 1"));

        let headless: String = session.lines().skip(1).map(|l| format!("{l}\n")).collect();
        let failure = check("pi", "headless.jsonl", &headless).failure.unwrap();
        assert_eq!(failure.check, Check::Discover, "{}", failure.message);
    }

    /// Lines both readers take agree whatever the form of their
    /// numbers, strings, spacing, and keys; so do a blank line, a torn
    /// line, and a line that is not UTF-8, which neither reads.
    #[test]
    fn lines_both_readers_read_alike_agree() {
        let content = concat!(
            r#"{"type":"user","n":[0.000005,1E3,1.50,-0,-0.0,100000000000000000000,1e-7]}"#,
            "\n\n  \r\n",
            r#"{ "b" : "x\n\u00e9\ud83d\ude00" , "a" : 1 , "b" : 2 }"#,
            "\n",
            r#"{"type":"user","message":{"content":"Exit co{"type":"last-prompt"}"#,
            "\n",
            r#"{"type":"user","text":"cut \ud83d"}"#,
            "\n",
            r#"{"n":1e999}"#,
            "\n",
        );
        assert_eq!(reader_agreement(content.as_bytes()), Ok(()));
        assert_eq!(reader_agreement(b"{\"a\":\"\xff\"}\n"), Ok(()));
        assert_eq!(check("claude", "any-name.jsonl", CLAUDE).failure, None);
    }

    /// A line one reader takes and the other refuses, and a line the
    /// two read to different values, are differences. A number equal
    /// in value and written another way is not.
    #[test]
    fn a_line_the_readers_read_differently_is_a_difference() {
        let entry: EntryValue = r#"{"a":1.50,"b":[1E3]}"#.parse().unwrap();
        let same = serde_json::json!({"b": [1000.0], "a": 1.5});
        let other = serde_json::json!({"b": [1000.0], "a": 1.25});
        assert_eq!(same_reading(Some(&entry), Some(&same)), Ok(()));
        assert_eq!(same_reading(None, None), Ok(()));
        assert_eq!(
            same_reading(Some(&entry), Some(&other)),
            Err("the entry reader reads another value than serde_json")
        );
        assert_eq!(
            same_reading(Some(&entry), None),
            Err("the entry reader reads it and serde_json does not")
        );
        assert_eq!(
            same_reading(None, Some(&same)),
            Err("serde_json reads it and the entry reader does not")
        );
    }

    #[test]
    fn a_line_the_codec_writes_differently_fails_the_codec_check() {
        let spaced = CLAUDE.replacen(r#"{"type":"user","#, r#"{"type": "user","#, 1);
        let failure = check("claude", "spaced.jsonl", &spaced).failure.unwrap();
        assert_eq!(failure.check, Check::Codec, "{}", failure.message);
        assert_eq!(failure.message, "line 1 encodes to other bytes");
    }
}
