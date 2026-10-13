//! `gage backup`: send the whole store to a backup remote.
//!
//! Every session in each configured source is added to the store
//! first, so the backup covers what the sources hold as well as what
//! the store already had. Then every object, tag, and key ref goes. A
//! configured remote must carry the backup flag, which is the user's
//! standing statement that it may hold everything; the store's
//! `pre-push` hook accepts the push on that flag alone. A bare Git URL
//! or path carries no flag, so the confirmation that names it is the
//! statement, and the push carries the approval digest of the whole
//! set as `gage push` does.

use clap::Args;
use cliclack as cli;
use gage_core::config::Config;
use gage_store::{KEY_REFS, OBJECT_REFS, RemoteStore, Repository, Store, TAG_REFS};

use crate::cmd_session;
use crate::dialog::{self, DialogError};
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

pub async fn main(args: BackupArgs) {
    let config = match Config::load_user() {
        Ok(config) => config,
        Err(e) => fail(&e),
    };
    let sources: Vec<String> = if args.stored {
        Vec::new()
    } else if args.source.is_empty() {
        config.backup.sources.clone()
    } else {
        args.source.clone()
    };
    let max_bytes = config.storage.max_session_size.bytes();
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => fail(&e),
    };
    let remotes = RemoteStore::from(&store);
    let known = match remotes.list() {
        Ok(known) => known,
        Err(e) => fail(&e),
    };
    let backups: Vec<&str> = known
        .iter()
        .filter(|r| r.backup)
        .map(|r| r.name.as_str())
        .collect();
    let repository = match &args.repository {
        Some(arg) => match known.iter().find(|r| &r.name == arg) {
            Some(found) if found.backup => Repository::from_remote(arg),
            Some(_) => fail(&format!(
                "remote {arg} is not a backup target; add one with gage remote add --backup"
            )),
            None => Repository::from_url(arg),
        },
        None => match backups.as_slice() {
            [one] => Repository::from_remote(one),
            [] => {
                fail(&"a backup remote or URL is required; add one with gage remote add --backup")
            }
            many => fail(&format!(
                "a remote argument is required; one of: {}",
                many.join(", ")
            )),
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
            format!("{} keys", count(KEY_REFS)),
            format!("{} already on the remote", refs.len() - pending.len()),
        ];
        cli::log::remark(summary.join("\n"))?;
        send = pending;
        Ok(format!("Backing up {} refs to {remote}", send.len()).into())
    })
    .await;
    if send.is_empty() {
        return;
    }

    let approval = match remotes.approval_digest(&send) {
        Ok(approval) => approval,
        Err(e) => fail(&e),
    };
    let status = match remotes.push(&repository, &send, &approval) {
        Ok(status) => status,
        Err(e) => fail(&e),
    };
    if !status.success() {
        fail(&format!(
            "backup to {remote} was rejected; fetch and retry: gage fetch {remote}"
        ));
    }
    println!("Backed up {} refs to {remote}", send.len());
}

fn fail(e: &dyn std::fmt::Display) -> ! {
    eprintln!("gage backup: {e}");
    std::process::exit(1)
}
