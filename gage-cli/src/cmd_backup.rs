//! `gage backup`: send the whole store to a backup remote.
//!
//! Every object, tag, and key ref goes. The remote must carry the
//! backup flag, which is the user's statement that it may hold
//! everything; the store's `pre-push` hook accepts the push on that
//! flag alone, so no approval digest is involved.

use clap::Args;
use cliclack as cli;
use gage_store::{KEY_REFS, OBJECT_REFS, RemoteStore, Store, StoreError, TAG_REFS};

use crate::dialog::{self, DialogError};

#[derive(Args)]
pub struct BackupArgs {
    /// Backup remote to send the store to
    ///
    /// A remote added with `gage remote add --backup`. Optional when
    /// exactly one backup remote exists
    remote: Option<String>,

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
    let remote = match &args.remote {
        Some(name) => match known.iter().find(|r| &r.name == name) {
            Some(found) if found.backup => name.clone(),
            Some(_) => fail(&format!(
                "remote {name} is not a backup target; add one with gage remote add --backup"
            )),
            None => fail(&StoreError::RemoteNotFound(format!(
                "{name} (add one with gage remote add --backup)"
            ))),
        },
        None => match backups.as_slice() {
            [one] => (*one).to_string(),
            [] => fail(&"a backup remote is required; add one with gage remote add --backup"),
            many => fail(&format!(
                "a remote argument is required; one of: {}",
                many.join(", ")
            )),
        },
    };
    let refs = match remotes.gage_refs() {
        Ok(refs) => refs,
        Err(e) => fail(&e),
    };
    let on_remote = match remotes.ls_remote(&remote) {
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
        let status = remotes
            .push(&remote, &send, "")
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
