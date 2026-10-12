//! `gage backup`: send the whole store to a backup remote.
//!
//! Every object, tag, and key ref goes. A configured remote must carry
//! the backup flag, which is the user's standing statement that it may
//! hold everything; the store's `pre-push` hook accepts the push on
//! that flag alone. A bare Git URL or path carries no flag, so the
//! confirmation that names it is the statement, and the push carries
//! the approval digest of the whole set as `gage push` does.

use clap::Args;
use cliclack as cli;
use gage_store::{KEY_REFS, OBJECT_REFS, RemoteStore, Repository, Store, TAG_REFS};

use crate::dialog::{self, DialogError};

#[derive(Args)]
pub struct BackupArgs {
    /// Backup remote or URL
    ///
    /// A remote added with `gage remote add --backup`, or any Git URL
    /// or path. Optional when exactly one backup remote exists
    repository: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub fn main(args: BackupArgs) {
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
    let refs = match remotes.gage_refs() {
        Ok(refs) => refs,
        Err(e) => fail(&e),
    };
    let on_remote = match remotes.ls_remote(&repository) {
        Ok(on_remote) => on_remote,
        Err(e) => fail(&e),
    };
    let send: Vec<String> = refs
        .iter()
        .filter(|(name, sha)| on_remote.get(*name) != Some(sha))
        .map(|(name, _)| name.clone())
        .collect();
    if send.is_empty() {
        println!("Everything up to date on {remote}");
        return;
    }
    let count = |prefix: &str| send.iter().filter(|r| r.starts_with(prefix)).count();
    let summary = [
        format!("{} objects", count(OBJECT_REFS)),
        format!("{} tags", count(TAG_REFS)),
        format!("{} keys", count(KEY_REFS)),
        format!("{} already on the remote", refs.len() - send.len()),
    ];

    dialog::run(&format!("Backup to {remote}"), || {
        cli::log::remark(summary.join("\n"))?;
        if !args.yes {
            let prompt = format!("Send {} refs to {remote}?", send.len());
            let confirmed = cli::confirm(prompt).initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }
        let approval = remotes
            .approval_digest(&send)
            .map_err(|e| DialogError::Other(e.into()))?;
        let status = remotes
            .push(&repository, &send, &approval)
            .map_err(|e| DialogError::Other(e.into()))?;
        if !status.success() {
            return Err(DialogError::Failed(format!(
                "Backup to {remote} was rejected; fetch and retry: gage fetch {remote}"
            )));
        }
        Ok(format!("Backed up {} refs to {remote}", send.len()).into())
    });
}

fn fail(e: &dyn std::fmt::Display) -> ! {
    eprintln!("gage backup: {e}");
    std::process::exit(1)
}
