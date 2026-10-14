//! `gage backup`: send the whole store to a backup remote.
//!
//! Every session in each configured source is added to the store
//! first, so the backup covers what the sources hold as well as what
//! the store already had. Then every object and tag ref goes. A
//! configured remote must carry the backup flag, which is the user's
//! standing statement that it may hold everything; the store's
//! `pre-push` hook accepts the push on that flag alone. A bare Git URL
//! or path carries no flag, so the confirmation that names it is the
//! statement, and the push carries the approval digest of the whole
//! set as `gage push` does.

use anyhow::Context;
use clap::Args;
use cliclack as cli;
use gage_core::config::Config;
use gage_store::{OBJECT_REFS, RemoteStore, Repository, Store, TAG_REFS};

use crate::cmd_session;
use crate::dialog::{self, DialogError};
use crate::error::CliError;
use crate::source::SOURCE_HELP;

#[derive(Args)]
pub struct BackupArgs {
    /// Backup remote or URL
    ///
    /// A remote added with `gage remote add --backup`, or any Git URL
    /// or path. Optional when exactly one backup remote exists
    repository: Option<String>,

    /// Add sessions from SOURCE before backup (repeatable)
    ///
    /// Replaces the configured backup.sources list for this run.
    #[arg(
        short,
        long,
        value_name = "SOURCE",
        long_help = format!("Add sessions from SOURCE before backup (repeatable)\n\n\
Replaces the configured backup.sources list for this run.\n\n{SOURCE_HELP}")
    )]
    source: Vec<String>,

    /// Back up stored sessions only
    ///
    /// Sends the store as it is; no source is read first.
    #[arg(short = 'S', long, conflicts_with = "source")]
    stored: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub async fn main(args: BackupArgs) -> Result<(), CliError> {
    let config = Config::load_user().context("gage backup")?;
    let sources: Vec<String> = if args.stored {
        Vec::new()
    } else if args.source.is_empty() {
        config.backup.sources.clone()
    } else {
        args.source.clone()
    };
    let max_bytes = config.storage.max_session_size.bytes();
    let store = Store::open(&gage_store::store_path()).context("gage backup")?;
    let remotes = RemoteStore::from(&store);
    let known = remotes.list().context("gage backup")?;
    let backups: Vec<&str> = known
        .iter()
        .filter(|r| r.backup)
        .map(|r| r.name.as_str())
        .collect();
    let repository = match &args.repository {
        Some(arg) => match known.iter().find(|r| &r.name == arg) {
            Some(found) if found.backup => Repository::from_remote(arg),
            Some(_) => {
                return Err(CliError::message(format!(
                    "gage backup: remote {arg} is not a backup target; add one with gage remote add --backup"
                )));
            }
            None => Repository::from_url(arg),
        },
        None => match backups.as_slice() {
            [one] => Repository::from_remote(one),
            [] => {
                return Err(CliError::message(
                    "gage backup: a backup remote or URL is required; add one with gage remote add --backup",
                ));
            }
            many => {
                return Err(CliError::message(format!(
                    "gage backup: a remote argument is required; one of: {}",
                    many.join(", ")
                )));
            }
        },
    };
    let remote = repository.name();

    // The dialog adds the sources, previews, and confirms; the push
    // runs after it, so git's own output lands outside the dialog
    let mut send: Vec<String> = Vec::new();
    dialog::run_async(&format!("Backup to {remote}"), || async {
        if !sources.is_empty() {
            dialog::step_list("Sources", sources.iter().cloned())?;
        }
        if !args.yes && !cli::confirm("Continue?").initial_value(false).interact()? {
            return Err(DialogError::Canceled);
        }
        for source in &sources {
            let added = cmd_session::add_all(&store, source, max_bytes).await?;
            cli::log::info(added)?;
        }
        let refs = remotes
            .gage_refs()
            .map_err(|e| DialogError::Other(e.into()))?;
        let on_remote = remotes
            .ls_remote(&repository)
            .map_err(|e| DialogError::Other(e.into()))?;
        let pending: Vec<String> = refs
            .iter()
            .filter(|(name, sha)| on_remote.get(*name) != Some(sha))
            .map(|(name, _)| name.clone())
            .collect();
        if pending.is_empty() {
            return Ok(format!("Everything up to date on {remote}").into());
        }
        let count = |prefix: &str| pending.iter().filter(|r| r.starts_with(prefix)).count();
        let summary = [
            format!("{} objects", count(OBJECT_REFS)),
            format!("{} tags", count(TAG_REFS)),
            format!("{} already on the remote", refs.len() - pending.len()),
        ];
        cli::log::remark(summary.join("\n"))?;
        send = pending;
        Ok(format!("Backing up {} refs to {remote}", send.len()).into())
    })
    .await?;
    if send.is_empty() {
        return Ok(());
    }

    let approval = remotes.approval_digest(&send).context("gage backup")?;
    let status = remotes
        .push(&repository, &send, &approval)
        .context("gage backup")?;
    if !status.success() {
        return Err(CliError::message(format!(
            "gage backup: backup to {remote} was rejected; fetch and retry: gage fetch {remote}"
        )));
    }
    println!("Backed up {} refs to {remote}", send.len());
    Ok(())
}
