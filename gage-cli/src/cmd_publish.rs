//! `gage session publish`: the pending-publish life cycle commands.
//!
//! `init` stages a publish of a session; `redact`, `status`, `undo`,
//! `abort`, and `apply` follow. The source session is resolved
//! through the session command's `--source` and `--stored`, the way
//! `gage scan` resolves sessions: a `--stored` session is used as is,
//! and a native session is added to the store first so it has an
//! object id and commit for `origin`.

use clap::{Args, Subcommand};
use gage_core::uuid::short_uuid;
use gage_session::SourceOptions;
use gage_store::{PublishStore, SessionRecord, SessionStore, Store};

use crate::error::CliError;
use crate::source;

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

/// List the pending publish working trees, reading the file system.
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
    for p in pending {
        let title = p.attrs.summary.title.as_deref().unwrap_or("");
        println!("{}  {}", short_uuid(&p.id), title);
    }
    Ok(())
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
