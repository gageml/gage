//! A corpus for this crate's tests: session files are
//! `<root>/<project>/<id>.jsonl`, and each line is a JSON object
//! whose `type` is the entry and message type and whose
//! `message.content` string is the message text.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gage_session::{Entry, Message};
use serde_json::Value;

use crate::{
    Corpus, CorpusSessionAttrs, DerivedRows, DerivedSession, Fingerprint, Result, SessionInfo,
};

#[derive(Debug)]
pub(crate) struct LineCorpus {
    root: PathBuf,
}

impl LineCorpus {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }
}

impl Corpus for LineCorpus {
    fn driver(&self) -> &'static str {
        "lines"
    }

    fn session_type(&self) -> &'static str {
        "lines"
    }

    fn content_format(&self) -> &'static str {
        "lines 1"
    }

    fn summary_version(&self) -> u32 {
        1
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn sessions_dir(&self) -> &Path {
        &self.root
    }

    fn sessions(&self, _show_agents: bool) -> io::Result<Vec<SessionInfo>> {
        let mut out = Vec::new();
        for project in std::fs::read_dir(&self.root)? {
            let project = project?.path();
            if !project.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(&project)? {
                let path = file?.path();
                let Some(id) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".jsonl"))
                else {
                    continue;
                };
                let meta = path.metadata()?;
                out.push(SessionInfo {
                    id: id.to_string(),
                    mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    size: meta.len(),
                    project: project
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    src: path,
                });
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.mtime));
        Ok(out)
    }

    fn derive(&self, id: &str, path: &Path) -> Result<DerivedSession> {
        let fingerprint = Fingerprint::stat(path)?;
        let text = std::fs::read_to_string(path)?;
        let mut rows = DerivedRows::new();
        let mut attrs = CorpusSessionAttrs {
            native_mtime_ms: fingerprint.mtime_ms,
            native_size: fingerprint.size,
            ..Default::default()
        };
        for (i, line) in text.lines().enumerate() {
            let value: Value = serde_json::from_str(line)?;
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let content = value
                .pointer("/message/content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let line_num = i as u32 + 1;
            attrs.line_count = u64::from(line_num);
            attrs.message_count += 1;
            rows.push(
                id,
                &Entry {
                    line: line_num,
                    uuid: None,
                    entry_type: kind.to_string(),
                    subtype: None,
                    timestamp_ms: None,
                    raw: Some(line.to_string()),
                    message: Some(Message {
                        message_type: kind.to_string(),
                        subtype: Some("text".to_string()),
                        text: content.to_string(),
                        attachments: None,
                        ide_tags: None,
                    }),
                },
            );
        }
        DerivedSession::new(id, rows, Vec::new(), attrs, fingerprint)
    }
}
