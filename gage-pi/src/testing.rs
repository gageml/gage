//! Sessions for this crate's tests. The hand-built ones under
//! `testdata/` are small files in the shapes Pi has written over its
//! format versions; every id and path in them is made up. The ones
//! under `fixtures/` were written by Pi itself.

use std::fs;
use std::path::{Path, PathBuf};

use gage_corpus::{DerivedSession, Fingerprint, Result, SessionReader};
use gage_session::ToolUse;
use serde_json::Value;

use crate::codec::codec;
use crate::derive::derive_session_from;
use crate::tool_use::ToolUseCollector;

/// Format version 1 with the header fields the spec leaves out, a
/// compaction that names its kept entry by index, an extension message
/// under the `hookMessage` role, a blank line (5), and a torn line (9)
pub(crate) const V1_EXTENDED: &str = include_str!("../testdata/v1-extended.jsonl");

/// [`V1_EXTENDED`] as Pi 1.5.2 rewrote it on opening it: format
/// version 3, two lines shorter
pub(crate) const V1_EXTENDED_MIGRATED: &str = include_str!(
    "../fixtures/pi-1.5.2/migrated/2025-06-01T10-00-00-000Z_11111111-1111-4111-8111-111111111111.jsonl"
);

/// The session id of [`V1_EXTENDED`]
pub(crate) const V1_EXTENDED_ID: &str = "11111111-1111-4111-8111-111111111111";

/// Format version 1 with nothing but the documented fields
pub(crate) const V1_PLAIN: &str = include_str!("../testdata/v1-plain.jsonl");

/// Format version 3 with two branches from line 3 and a branch summary
pub(crate) const V3_BRANCH: &str = include_str!("../testdata/v3-branch.jsonl");

/// A version 3 header over entries with no ids, as a fork of a
/// version 1 file is written
pub(crate) const V3_HEADER_V1_ENTRIES: &str =
    include_str!("../testdata/v3-header-v1-entries.jsonl");

/// Format version 3 with a torn line (3) and a blank line (4)
pub(crate) const V3_TORN: &str = include_str!("../testdata/v3-torn.jsonl");

/// Pi's own statistics for each session it wrote, by the session's
/// name: `run`, an ordinary agent run; `branches`, a session with two
/// branches; and `fork`, a fork of `run` into another project.
pub(crate) fn pi_stats() -> Value {
    serde_json::from_str(include_str!("../fixtures/pi-1.5.2/stats.json")).unwrap()
}

/// The session Pi wrote under `name`, as `(session id, text)`.
pub(crate) fn generated(name: &str) -> (String, String) {
    let stats = pi_stats();
    let file = stats[name]["file"].as_str().unwrap();
    let text = fs::read_to_string(generated_agent_dir().join("sessions").join(file)).unwrap();
    (stats[name]["sessionId"].as_str().unwrap().to_string(), text)
}

/// The names of the sessions Pi wrote
pub(crate) const GENERATED: [&str; 3] = ["run", "branches", "fork"];

/// The agent directory holding the sessions Pi wrote
pub(crate) fn generated_agent_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/pi-1.5.2")
}

/// The lines of `text`, parsed, in order; a blank or torn line is
/// `Null`, so line `n` is at `n - 1`.
pub(crate) fn lines(text: &str) -> Vec<Value> {
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap_or(Value::Null))
        .collect()
}

/// Derive the session `text` holds, as the session `id` of the
/// project `-w-proj`.
pub(crate) fn try_derive(id: &str, text: &str) -> Result<DerivedSession> {
    let fingerprint = Fingerprint {
        mtime_ms: 1,
        size: text.len() as u64,
    };
    derive_session_from(
        id,
        SessionReader::new(text.as_bytes(), codec()),
        fingerprint,
        "-w-proj".to_string(),
    )
}

pub(crate) fn derive(text: &str) -> DerivedSession {
    try_derive("s", text).unwrap()
}

/// The `tool_use` rows of the session `text` holds.
pub(crate) fn tool_uses_of(text: &str) -> Vec<ToolUse> {
    let mut collector = ToolUseCollector::new();
    for item in SessionReader::new(text.as_bytes(), codec()) {
        let (line_num, _, decoded) = item.unwrap();
        collector.push(line_num, &Value::from(decoded));
    }
    collector.finish()
}

/// Write `text` under the sessions directory `sessions_dir` as the
/// session `id` of a project, where Pi would have filed it, and
/// return its path.
pub(crate) fn seed(sessions_dir: &Path, id: &str, text: &str) -> PathBuf {
    let dir = sessions_dir.join("--w-proj--");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("2025-06-01T10-00-00-000Z_{id}.jsonl"));
    fs::write(&path, text).unwrap();
    path
}
