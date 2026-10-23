//! `gage session publish`: the pending-publish life cycle commands.
//!
//! `init` stages a publish of a session; `redact`, `status`, `undo`,
//! `abort`, and `apply` follow. The source session is resolved
//! through the session command's `--source` and `--stored`, the way
//! `gage scan` resolves sessions: a `--stored` session is used as is,
//! and a native session is added to the store first so it has an
//! object id and commit for `origin`.

use std::cmp::Reverse;

use clap::{Args, Subcommand};
use gage_core::style::IdHighlighter;
use gage_session::SourceOptions;
use gage_store::{PendingPublish, PublishStore, SESSION_TYPE, SessionRecord, SessionStore, Store};
use tabled::{
    Table,
    settings::{
        Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::error::CliError;
use crate::human::format_elapsed_ms;
use crate::source;
use crate::style;

#[derive(Subcommand)]
pub enum PublishCommand {
    /// Start a publish of a session
    ///
    /// Stages a pending publish and prints the new session ID, the
    /// value the other publish commands take as their SESSION
    /// argument.
    Init(InitArgs),

    /// List pending publish sessions
    List,
}

#[derive(Args)]
pub struct InitArgs {
    /// Session to publish (tag, ID, or prefix)
    #[arg(value_name = "SESSION")]
    session: String,
}

pub async fn run(
    source: Option<String>,
    stored: bool,
    command: PublishCommand,
) -> Result<(), CliError> {
    match command {
        PublishCommand::Init(args) => init(source, stored, args),
        PublishCommand::List => list(),
    }
}

/// List the pending publish working trees, read from the file system,
/// newest first. Ids are highlighted against the stored sessions plus
/// the pending set, the peers a pending prefix resolves against.
fn list() -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish list: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let pending = PublishStore::from(&store)
        .list()
        .map_err(|e| fail(e.to_string()))?;
    if pending.is_empty() {
        println!("No pending publish sessions");
        return Ok(());
    }
    let mut peers = store
        .short_prefix_ids(Some(SESSION_TYPE))
        .map_err(|e| fail(e.to_string()))?;
    peers.extend(pending.iter().map(|p| p.id.clone()));
    let highlighter = IdHighlighter::new(peers);

    let header: Vec<String> = ["Id", "Origin", "Title", "Modified"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = pending_rows(pending, &highlighter);
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(2..4).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    Ok(())
}

/// One row per pending publish, newest first: the highlighted short
/// id, the `origin` and title from `attrs.json`, and the age of the
/// working tree.
fn pending_rows(mut pending: Vec<PendingPublish>, highlighter: &IdHighlighter) -> Vec<Vec<String>> {
    pending.sort_by_key(|p| Reverse(p.modified_ms));
    pending
        .iter()
        .map(|p| {
            vec![
                highlighter.short(&p.id),
                origin_cell(&p.attrs.origin, highlighter),
                p.attrs.summary.title.clone().unwrap_or_default(),
                format_elapsed_ms(p.modified_ms),
            ]
        })
        .collect()
}

/// A derived origin, `session:<id>@<commit>`, as the highlighted short
/// id of the original session, a space, and the first 8 characters of
/// the commit dimmed. Any other origin is shown as stored.
fn origin_cell(origin: &str, highlighter: &IdHighlighter) -> String {
    let Some((id, commit)) = origin
        .strip_prefix("session:")
        .and_then(|rest| rest.split_once('@'))
    else {
        return origin.to_string();
    };
    let commit: String = commit.chars().take(8).collect();
    format!("{} {}", highlighter.short(id), console::style(commit).dim())
}

fn init(source: Option<String>, stored: bool, args: InitArgs) -> Result<(), CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish init: {e}"));
    let store = Store::open(&gage_store::store_path()).map_err(|e| fail(e.to_string()))?;
    let original = resolve_original(&store, source.as_deref(), stored, &args.session)?;
    let outcome = PublishStore::from(&store)
        .init(&original)
        .map_err(|e| fail(e.to_string()))?;
    println!("{}", outcome.id);
    Ok(())
}

/// The stored session to publish. `--stored` resolves it in the
/// store; otherwise it is a native session in `source`, added to the
/// store first so it has an object id and commit for `origin`.
fn resolve_original(
    store: &Store,
    source: Option<&str>,
    stored: bool,
    prefix: &str,
) -> Result<SessionRecord, CliError> {
    let fail = |e: String| CliError::message(format!("gage session publish init: {e}"));
    let sessions = SessionStore::from(store);
    if stored {
        return sessions
            .get(prefix)
            .map_err(|e| fail(format!("{prefix}: {e}")));
    }
    let registry = source::driver_registry();
    let (driver, spec) = source::resolve_source(&registry, source).map_err(fail)?;
    let opened = driver
        .open_source(&SourceOptions::new(&spec))
        .map_err(|e| fail(format!("{spec}: {e}")))?;
    let native_id = opened
        .find_native(prefix)
        .map_err(|e| fail(format!("{prefix}: {e}")))?;
    let mut native = opened
        .open_native(&native_id)
        .map_err(|e| fail(format!("{native_id}: {e}")))?;
    let outcome = sessions
        .add(driver.as_ref(), native.as_mut())
        .map_err(|e| fail(e.to_string()))?;
    sessions
        .at_commit(&outcome.commit_sha)
        .map_err(|e| fail(e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gage_core::datetime::now_ms;
    use gage_core::uuid::short_uuid;
    use gage_store::{SessionAttrsRecord, SummaryAttrs};

    use super::*;

    fn pending(id: &str, origin: &str, title: Option<&str>, modified_ms: i64) -> PendingPublish {
        PendingPublish {
            id: id.to_string(),
            attrs: SessionAttrsRecord {
                driver: "claude 1".to_string(),
                origin: origin.to_string(),
                native_mtime: 0,
                native_size: 0,
                session_type: "claude".to_string(),
                content_format: "claude-jsonl 1".to_string(),
                size: 0,
                project: None,
                summary: SummaryAttrs {
                    title: title.map(String::from),
                    ..SummaryAttrs::default()
                },
            },
            root: PathBuf::from("/tmp/publishes").join(id),
            modified_ms,
        }
    }

    /// Rows come newest first and carry the short id, the origin, the
    /// title, and the age.
    #[test]
    fn pending_rows_are_newest_first_with_id_origin_and_age() {
        console::set_colors_enabled(false);
        let now = now_ms();
        let orig1 = "cccccccc-0000-4000-8000-000000000003";
        let orig2 = "dddddddd-0000-4000-8000-000000000004";
        let older = pending(
            "aaaaaaaa-0000-4000-8000-000000000001",
            &format!("session:{orig1}@0123456789abcdef0123456789abcdef01234567"),
            None,
            now - 3_600_000,
        );
        let newer = pending(
            "bbbbbbbb-0000-4000-8000-000000000002",
            &format!("session:{orig2}@fedcba9876543210fedcba9876543210fedcba98"),
            Some("Fix the duck"),
            now - 5_000,
        );
        let highlighter = IdHighlighter::new(vec![
            older.id.clone(),
            newer.id.clone(),
            orig1.to_string(),
            orig2.to_string(),
        ]);

        let rows = pending_rows(vec![older.clone(), newer.clone()], &highlighter);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], short_uuid(&newer.id));
        assert_eq!(rows[0][1], format!("{} fedcba98", short_uuid(orig2)));
        assert_eq!(rows[0][2], "Fix the duck");
        assert_eq!(rows[0][3], "5s ago");
        assert_eq!(rows[1][0], short_uuid(&older.id));
        assert_eq!(rows[1][1], format!("{} 01234567", short_uuid(orig1)));
        assert_eq!(rows[1][2], "");
        assert_eq!(rows[1][3], "1h ago");
    }

    /// An origin that is not a derived `session:<id>@<commit>` value is
    /// shown as stored.
    #[test]
    fn origin_cell_shows_a_native_origin_as_stored() {
        console::set_colors_enabled(false);
        let highlighter = IdHighlighter::new(vec![]);
        assert_eq!(
            origin_cell("claude:/home/u/.claude/abc", &highlighter),
            "claude:/home/u/.claude/abc"
        );
    }
}
