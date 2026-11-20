//! The session a Gage process runs in, as the drivers name it.
//!
//! A harness tells the processes it hosts which session they belong
//! to in its own way, so the question goes to the drivers: each is
//! asked through [`gage_session::Driver::current_session`], and the
//! first that sees one of its sessions answers.

use std::sync::Arc;

use gage_mcp::CurrentSessionFn;
use gage_registry::driver::DriverRegistry;
use gage_session::HostContext;

/// How the MCP server names the session a tool call comes from: by
/// asking `registry`'s drivers, each given this process's environment
/// and the call's request `_meta`.
pub fn current_session_fn(registry: Arc<DriverRegistry>) -> CurrentSessionFn {
    Arc::new(move |meta| {
        registry
            .current_session(&HostContext::process().serving_call(meta))
            .map_err(|e| e.to_string())
    })
}

/// The id of the session this process's environment names, as a
/// harness sets it for the commands a session runs. `None` when no
/// driver sees a session there.
pub fn environment_session_id(registry: &DriverRegistry) -> Option<String> {
    let url = registry
        .current_session(&HostContext::process())
        .ok()
        .flatten()?;
    registry.split_session_url(&url).ok().map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use gage_mcp::{GageTool, IssueAssignConfig, StoreWriterConfig, ToolSpec, serve_io};
    use gage_session::{
        ContentSink, ContentSource, Driver, DriverError, NativeSession, Source, SourceOptions,
        StoredSession, WrittenContent,
    };
    use gage_store::{IssueInput, IssueStatus, IssueStore, Store};
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    use super::*;

    /// A driver whose harness names the session only in a tool call's
    /// `_meta`, under `x-thread/id`, as Codex does
    struct PerCall;

    impl Driver for PerCall {
        fn name(&self) -> &'static str {
            "percall"
        }
        fn version(&self) -> &'static str {
            "0"
        }
        fn schemes(&self) -> &'static [&'static str] {
            &["percall"]
        }
        fn open_source(&self, _options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn split_session_url(&self, session_url: &str) -> Result<(String, String), DriverError> {
            let id = session_url
                .strip_prefix("percall:")
                .ok_or_else(|| DriverError::Other(format!("{session_url}: not a percall URL")))?;
            Ok(("percall:".to_string(), id.to_string()))
        }
        fn write_native(
            &self,
            _session: &mut dyn NativeSession,
            _sink: &mut dyn ContentSink,
        ) -> Result<WrittenContent, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn read_stored(
            &self,
            _native_id: String,
            _content_format: &str,
            _source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn current_session(&self, host: &HostContext<'_>) -> Result<Option<String>, DriverError> {
            Ok(host
                .call_meta()
                .and_then(|meta| meta.get("x-thread/id"))
                .and_then(Value::as_str)
                .map(|id| format!("percall:{id}")))
        }
    }

    fn registry() -> Arc<DriverRegistry> {
        Arc::new(DriverRegistry::new().add_default(Arc::new(PerCall)))
    }

    /// A store with one open issue, and the issue's id
    fn store_with_issue() -> (tempfile::TempDir, StoreWriterConfig, String) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open_with_cache_root(&path, &tmp.path().join("cache")).unwrap();
        let id = IssueStore::from(&store)
            .create(IssueInput {
                name: "general",
                title: "Something",
                description: None,
                author: "task:general:main",
                status: IssueStatus::Open,
                evidence: &[],
                key: None,
            })
            .unwrap();
        let writer = StoreWriterConfig {
            store: Arc::new(Mutex::new(store)),
            author: "user:t".into(),
        };
        (tmp, writer, id)
    }

    /// The client end of an MCP connection: JSON messages, one a line
    struct Client {
        lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<DuplexStream>>>,
        writer: tokio::io::WriteHalf<DuplexStream>,
        next_id: u64,
    }

    impl Client {
        async fn notify(&mut self, message: Value) {
            let mut line = message.to_string();
            line.push('\n');
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }

        /// Send a request and return the response's `result`
        async fn request(&mut self, method: &str, params: Value) -> Value {
            self.next_id += 1;
            let id = self.next_id;
            self.notify(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await;
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("server closed");
                let message: Value = serde_json::from_str(&line).unwrap();
                if message.get("id") == Some(&json!(id)) {
                    assert!(message.get("error").is_none(), "{method}: {message}");
                    return message["result"].clone();
                }
            }
        }
    }

    fn text_of(result: &Value) -> String {
        result["content"][0]["text"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn issue_assign_names_its_session_from_each_call_through_the_server() {
        let (_tmp, writer, issue) = store_with_issue();
        let spec = ToolSpec {
            gage: vec![GageTool::IssueAssign(IssueAssignConfig {
                writer: writer.clone(),
                current_session: current_session_fn(registry()),
            })],
            custom: Vec::new(),
        };
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_end);
        let (client_read, client_write) = tokio::io::split(client_end);
        let mut client = Client {
            lines: BufReader::new(client_read).lines(),
            writer: client_write,
            next_id: 0,
        };

        let calls = async {
            client
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": {"name": "test", "version": "0"},
                    }),
                )
                .await;
            client
                .notify(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .await;

            let tools = client.request("tools/list", json!({})).await;
            let names: Vec<&str> = tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap())
                .collect();
            assert_eq!(names, ["issue_assign"]);

            // No `_meta`: the harness named no session
            let refused = client
                .request(
                    "tools/call",
                    json!({"name": "issue_assign", "arguments": {"issue": issue}}),
                )
                .await;
            assert_eq!(refused["isError"], true, "{refused}");
            assert!(
                text_of(&refused).contains("could not identify this session"),
                "{refused}"
            );
            assert!(
                text_of(&refused).contains("The harness (test 0)"),
                "{refused}"
            );

            let assigned = client
                .request(
                    "tools/call",
                    json!({
                        "name": "issue_assign",
                        "arguments": {"issue": issue},
                        "_meta": {"x-thread/id": "t1"},
                    }),
                )
                .await;
            assert_eq!(assigned["isError"], false, "{assigned}");
            assert!(text_of(&assigned).starts_with("Assigned"), "{assigned}");

            // Another session's call is refused: the holder is the
            // session the earlier call named
            let other = client
                .request(
                    "tools/call",
                    json!({
                        "name": "issue_assign",
                        "arguments": {"issue": issue},
                        "_meta": {"x-thread/id": "t2"},
                    }),
                )
                .await;
            assert_eq!(other["isError"], true, "{other}");
            assert!(text_of(&other).contains("set force"), "{other}");
            drop(client);
        };
        let serve = async {
            serve_io(spec, server_read, server_write)
                .await
                .map_err(|e| e.to_string())
        };
        let (_, served) = tokio::join!(calls, serve);
        served.unwrap();

        let store = writer.store.lock().unwrap();
        let full = IssueStore::from(&*store).get(&issue).unwrap();
        assert_eq!(full.status, IssueStatus::Assigned);
        assert_eq!(
            full.changes.last().unwrap().message.as_deref(),
            Some("percall:t1")
        );
    }

    #[test]
    fn environment_session_id_is_none_when_no_driver_sees_a_session() {
        assert_eq!(environment_session_id(&registry()), None);
    }
}
