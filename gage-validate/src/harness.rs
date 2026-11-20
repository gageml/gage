//! What the suite knows about each harness beyond its driver: where a
//! session file goes in the harness's own layout, and where an entry
//! records the harness version.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;
use uuid::Uuid;

pub trait Harness: Sync {
    /// The name of the harness's driver, which is also the scheme of
    /// its source URLs
    fn driver(&self) -> &'static str;

    /// Copy `file` into the harness root `root` as the session `id`,
    /// where the driver finds it, and return the copy's path.
    fn stage(&self, root: &Path, id: &str, file: &Path) -> io::Result<PathBuf>;

    /// The harness version `entry` records, when it records one
    fn version(&self, entry: &Value) -> Option<String>;
}

/// The harness of the driver `name`, when the suite stages its files
pub fn harness(name: &str) -> Option<&'static dyn Harness> {
    match name {
        "claude" => Some(&Claude),
        "codex" => Some(&Codex),
        "pi" => Some(&Pi),
        _ => None,
    }
}

/// The session id a file of `source` is staged under: one id per
/// file, the same on every run.
pub fn session_id(source: &str, file: &Path) -> String {
    let key = format!("gage-validate:{source}/{}", file.display());
    Uuid::new_v5(&Uuid::NAMESPACE_URL, key.as_bytes())
        .hyphenated()
        .to_string()
}

/// The first harness version an entry of `content` records. A line
/// that is not JSON records none; the codec check is what reports it.
pub fn harness_version(harness: &dyn Harness, content: &[u8]) -> Option<String> {
    for line in content.split(|b| *b == b'\n') {
        if let Ok(entry) = serde_json::from_slice::<Value>(line)
            && let Some(version) = harness.version(&entry)
        {
            return Some(version);
        }
    }
    None
}

struct Claude;

impl Harness for Claude {
    fn driver(&self) -> &'static str {
        "claude"
    }

    fn stage(&self, root: &Path, id: &str, file: &Path) -> io::Result<PathBuf> {
        copy_into(
            file,
            &root.join("projects/-validate"),
            &format!("{id}.jsonl"),
        )
    }

    fn version(&self, entry: &Value) -> Option<String> {
        entry.get("version")?.as_str().map(str::to_string)
    }
}

struct Codex;

impl Harness for Codex {
    fn driver(&self) -> &'static str {
        "codex"
    }

    fn stage(&self, root: &Path, id: &str, file: &Path) -> io::Result<PathBuf> {
        copy_into(
            file,
            &root.join("sessions/2026/01/01"),
            &format!("rollout-2026-01-01T00-00-00-{id}.jsonl"),
        )
    }

    fn version(&self, entry: &Value) -> Option<String> {
        entry
            .get("payload")?
            .get("cli_version")?
            .as_str()
            .map(str::to_string)
    }
}

struct Pi;

impl Harness for Pi {
    fn driver(&self) -> &'static str {
        "pi"
    }

    fn stage(&self, root: &Path, id: &str, file: &Path) -> io::Result<PathBuf> {
        copy_into(
            file,
            &root.join("sessions/--validate--"),
            &format!("2026-01-01T00-00-00-000Z_{id}.jsonl"),
        )
    }

    /// Pi records no release in a session file. The header names the
    /// version of the session format, which is reported in its place;
    /// a header from before the format was versioned names none and is
    /// version 1.
    fn version(&self, entry: &Value) -> Option<String> {
        if entry.get("type")?.as_str()? != "session" {
            return None;
        }
        let version = entry.get("version").and_then(Value::as_u64).unwrap_or(1);
        Some(format!("format-v{version}"))
    }
}

fn copy_into(file: &Path, dir: &Path, name: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let staged = dir.join(name);
    fs::copy(file, &staged)?;
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_keeps_its_session_id_and_shares_it_with_no_other() {
        let id = session_id("s", Path::new("a/b.jsonl"));
        assert_eq!(id, session_id("s", Path::new("a/b.jsonl")));
        assert_ne!(id, session_id("s", Path::new("a/c.jsonl")));
        assert_ne!(id, session_id("t", Path::new("a/b.jsonl")));
        assert_eq!(id.len(), 36);
    }

    #[test]
    fn the_version_is_the_first_one_an_entry_records() {
        let claude =
            b"not json\n{\"type\":\"summary\"}\n{\"version\":\"2.1.85\"}\n{\"version\":\"3\"}\n";
        assert_eq!(harness_version(&Claude, claude).as_deref(), Some("2.1.85"));
        let codex = b"{\"type\":\"session_meta\",\"payload\":{\"cli_version\":\"0.118.0\"}}\n";
        assert_eq!(harness_version(&Codex, codex).as_deref(), Some("0.118.0"));
        assert_eq!(harness_version(&Codex, claude), None);
    }

    #[test]
    fn a_pi_session_reports_its_format_version() {
        let current =
            b"\n{\"type\":\"session\",\"version\":3,\"id\":\"s\"}\n{\"type\":\"message\"}\n";
        assert_eq!(harness_version(&Pi, current).as_deref(), Some("format-v3"));
        let first = b"{\"type\":\"session\",\"id\":\"s\"}\n";
        assert_eq!(harness_version(&Pi, first).as_deref(), Some("format-v1"));
        assert_eq!(harness_version(&Pi, b"{\"type\":\"message\"}\n"), None);
    }
}
