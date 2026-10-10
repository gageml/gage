//! The store-side forms of `IssueWrite` and `NoteWrite`. The runtime's
//! forms write into a scan directory under a task; these write the
//! store directly, as `gage mcp2` serves an interactive session, with
//! the writer's author on each object.

use std::collections::BTreeMap;
use std::sync::Arc;

use gage_core::uuid::short_uuid;
use gage_store::{
    IssueInput, IssueStatus, IssueStore, NoteInput, NoteStore, NoteValue, SESSION_TYPE, Store,
};
use serde_json::Value;

use crate::service::{
    CustomToolOutcome, IssueWriteConfig, IssueWriteInput, NoteWriteConfig, NoteWriteInput,
    StoreWriterConfig,
};

impl IssueWriteConfig {
    /// Issues written to the store under `name` with status `status`,
    /// by the writer's author.
    pub fn with_store(writer: StoreWriterConfig, name: String, status: IssueStatus) -> Self {
        IssueWriteConfig {
            callback: Arc::new(move |input: IssueWriteInput| {
                let writer = writer.clone();
                let name = name.clone();
                Box::pin(async move { write_issue(&writer, &name, status, input) })
            }),
        }
    }
}

/// Create the issue. A store refusal, such as a cited note that does
/// not exist, is an error result the model reads.
fn write_issue(
    writer: &StoreWriterConfig,
    name: &str,
    status: IssueStatus,
    input: IssueWriteInput,
) -> CustomToolOutcome {
    let mut cited = input.evidence;
    cited.sort();
    cited.dedup();
    let store = writer.store();
    let result = IssueStore::from(&*store).create(IssueInput {
        name,
        title: &input.title,
        description: input.description.as_deref(),
        author: &writer.author,
        status,
        evidence: &cited,
        key: None,
    });
    match result {
        Ok(id) => CustomToolOutcome::Success(Value::String(format!(
            "Wrote {status} issue {} ({}) with {} evidence note(s).",
            short_uuid(&id),
            input.title,
            cited.len()
        ))),
        Err(e) => CustomToolOutcome::Error(e.to_string()),
    }
}

impl NoteWriteConfig {
    /// Notes written to the store under one of `names`, by the
    /// writer's author.
    pub fn with_store(writer: StoreWriterConfig, names: BTreeMap<String, String>) -> Self {
        NoteWriteConfig {
            names,
            callback: Arc::new(move |input: NoteWriteInput| {
                let writer = writer.clone();
                Box::pin(async move { write_note(&writer, input) })
            }),
        }
    }
}

/// Create the note on the session the call names, or on nothing. A
/// session that does not resolve, or lines without a session, is an
/// error result the model reads.
fn write_note(writer: &StoreWriterConfig, input: NoteWriteInput) -> CustomToolOutcome {
    let store = writer.store();
    let target = match session_target(
        &store,
        input.session_id.as_deref(),
        input.session_lines.as_deref(),
    ) {
        Ok(target) => target,
        Err(message) => return CustomToolOutcome::Error(message),
    };
    let result = NoteStore::from(&*store).create(NoteInput {
        name: &input.name,
        value: NoteValue::Text(input.value),
        author: &writer.author,
        target: target.as_deref(),
        metadata: None,
        work_id: None,
    });
    match result {
        Ok(id) => CustomToolOutcome::Success(Value::String(match target {
            Some(target) => format!("Wrote {} note {} on {target}.", input.name, short_uuid(&id)),
            None => format!("Wrote {} note {}.", input.name, short_uuid(&id)),
        })),
        Err(e) => CustomToolOutcome::Error(e.to_string()),
    }
}

/// The target URL for a session given by id or unique prefix, with
/// the line selection when given; `None` when no session is given.
fn session_target(
    store: &Store,
    session: Option<&str>,
    lines: Option<&str>,
) -> Result<Option<String>, String> {
    match (session, lines) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err("`session_lines` requires `session_id`".to_string()),
        (Some(prefix), lines) => {
            let found = store
                .resolve_in(prefix, Some(SESSION_TYPE))
                .map_err(|e| format!("session_id: {e}"))?;
            if found.deleted {
                return Err(format!("session_id: session {} is deleted", found.id));
            }
            Ok(Some(match lines {
                Some(lines) => format!("session:{}#{lines}", found.id),
                None => format!("session:{}", found.id),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use gage_store::{IssueStatus, IssueStore, NoteStore, NoteValue, Order};

    use super::*;
    use crate::tools::store_test::config;

    #[tokio::test]
    async fn issue_write_creates_the_issue_under_the_configured_name_and_author() {
        let (_tmp, writer) = config();
        let tool =
            IssueWriteConfig::with_store(writer.clone(), "user-issue".into(), IssueStatus::Open);
        let outcome = (tool.callback)(IssueWriteInput {
            title: "Flaky build".into(),
            description: Some("It flakes.".into()),
            evidence: Vec::new(),
        })
        .await;
        let CustomToolOutcome::Success(Value::String(text)) = outcome else {
            panic!("unexpected outcome {outcome:?}");
        };
        assert!(text.starts_with("Wrote open issue "), "{text}");
        let store = writer.store();
        let issues: Vec<_> = IssueStore::from(&*store)
            .iter()
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].name, "user-issue");
        assert_eq!(issues[0].title, "Flaky build");
        assert_eq!(issues[0].author, "user:t");
        assert_eq!(issues[0].status, IssueStatus::Open);
        drop(store);

        let outcome = (tool.callback)(IssueWriteInput {
            title: "Cites nothing real".into(),
            description: None,
            evidence: vec!["nope".into()],
        })
        .await;
        assert!(
            matches!(outcome, CustomToolOutcome::Error(_)),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn note_write_creates_the_note_and_refuses_a_bad_target() {
        let (_tmp, writer) = config();
        let tool = NoteWriteConfig::with_store(
            writer.clone(),
            BTreeMap::from([("comment".to_string(), "A remark".to_string())]),
        );
        let outcome = (tool.callback)(NoteWriteInput {
            name: "comment".into(),
            value: "noted".into(),
            session_id: None,
            session_lines: None,
        })
        .await;
        let CustomToolOutcome::Success(Value::String(text)) = outcome else {
            panic!("unexpected outcome {outcome:?}");
        };
        assert!(text.starts_with("Wrote comment note "), "{text}");
        {
            let store = writer.store();
            let notes: Vec<_> = NoteStore::from(&*store)
                .query()
                .order(Order::ModifiedDesc)
                .iter()
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(notes.len(), 1);
            assert_eq!(notes[0].name, "comment");
            assert_eq!(notes[0].author, "user:t");
            assert_eq!(notes[0].value, NoteValue::Text("noted".into()));
            assert_eq!(notes[0].target, None);
        }
        for (session_id, session_lines) in [(None, Some("12")), (Some("nope"), None)] {
            let outcome = (tool.callback)(NoteWriteInput {
                name: "comment".into(),
                value: "x".into(),
                session_id: session_id.map(str::to_string),
                session_lines: session_lines.map(str::to_string),
            })
            .await;
            assert!(
                matches!(outcome, CustomToolOutcome::Error(_)),
                "{outcome:?}"
            );
        }
    }
}
