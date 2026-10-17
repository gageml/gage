//! The Gage tools. Each module exports `pub const TOOL: ToolDef`
//! built from its companion `.md` under `config/tools/`.

pub mod issue_assign;
pub mod issue_comment;
pub mod issue_pending_resolve;
pub mod issue_update;
pub mod issue_write;
pub mod issues_free;
pub mod note_write;
pub mod query;

/// Fixtures shared by the tests of the tools that write the store
#[cfg(test)]
pub(crate) mod store_test {
    use std::sync::{Arc, Mutex};

    use gage_store::{IssueInput, IssueStatus, IssueStore, Store};
    use rmcp::model::{CallToolResult, JsonObject};

    use crate::service::StoreWriterConfig;

    /// A fresh store under a temporary directory, kept alive by the
    /// returned guard, with `user:t` as the author
    pub(crate) fn config() -> (tempfile::TempDir, StoreWriterConfig) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open(&path).unwrap();
        let config = StoreWriterConfig {
            store: Arc::new(Mutex::new(store)),
            author: "user:t".into(),
        };
        (tmp, config)
    }

    pub(crate) fn issue(config: &StoreWriterConfig, title: &str, status: IssueStatus) -> String {
        let store = config.store.lock().unwrap();
        IssueStore::from(&*store)
            .create(IssueInput {
                name: "general",
                title,
                description: None,
                author: "task:general:main",
                status,
                evidence: &[],
                key: None,
            })
            .unwrap()
    }

    pub(crate) fn object(v: serde_json::Value) -> JsonObject {
        match v {
            serde_json::Value::Object(o) => o,
            other => panic!("not an object: {other}"),
        }
    }

    pub(crate) fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()
    }
}
