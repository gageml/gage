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
//!
//! The store is the authority over its backup. A ref the remote holds
//! at a commit outside the store's history is diverged, as every ref
//! is after a store is deleted and rebuilt from its sources; `--force`
//! replaces the remote's commit. A ref the remote holds and the store
//! does not is remote-only; `--prune` deletes it. Without the flags
//! both are listed and left alone, and diverged refs are an error
//! because the backup is incomplete.

use anyhow::Context;
use clap::Args;
use cliclack as cli;
use gage_core::config::Config;
use gage_core::uuid::short_uuid;
use gage_store::{
    OBJECT_REFS, PushSet, RemoteDiff, RemoteStore, Repository, Store, StoreError, TAG_REFS,
};

use crate::cmd_session;
use crate::dialog::{self, DialogError};
use crate::error::CliError;
use crate::human::count;
use crate::source::SOURCE_HELP;

/// Refs listed by name in the dialog before the rest is a count.
const LIST_CAP: usize = 10;

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

    /// Replace diverged objects and tags on remote
    ///
    /// Replace diverged objects and tags on remote. A diverged object is one the remote holds with a history the
    /// store's copy does not continue, as every object is after a
    /// store is deleted and rebuilt from its sources. Without this
    /// flag diverged objects are listed, left alone, and reported as
    /// an error once the rest is sent
    #[arg(long)]
    force: bool,

    /// Delete objects and tags only on remote
    ///
    /// Delete objects and tags only on remote: those the remote holds
    /// and the store does not. An object deleted in store is kept as a deletion record and is
    /// backed up, not pruned. Without this flag such objects and tags
    /// are listed and left alone
    #[arg(long)]
    prune: bool,

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
    let mut planned: Option<(RemoteDiff, PushSet)> = None;
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
        let diff = remotes
            .diff_remote(&repository)
            .map_err(|e| DialogError::Other(e.into()))?;
        let set = push_set(&diff, args.force, args.prune);
        cli::log::remark(summary_lines(&diff, &set).join("\n"))?;
        if !diff.diverged.is_empty() {
            let label = if args.force {
                "Replacing diverged refs"
            } else {
                "Diverged refs (not sent)"
            };
            let lines = local_ref_lines(&store, &diff.diverged)
                .map_err(|e| DialogError::Other(e.into()))?;
            dialog::step_list(label, capped(lines))?;
        }
        if !diff.remote_only.is_empty() {
            let label = if args.prune {
                "Deleting refs only on remote"
            } else {
                "Refs only on remote (kept)"
            };
            let lines = diff.remote_only.iter().map(|name| remote_ref_line(name));
            dialog::step_list(label, capped(lines.collect()))?;
        }
        let destructive = !set.force.is_empty() || !set.delete.is_empty();
        if destructive {
            cli::log::warning(effect_lines(set.force.len(), set.delete.len(), remote).join("\n"))?;
        }
        if destructive
            && !args.yes
            && !cli::confirm(replace_prompt(set.force.len(), set.delete.len(), remote))
                .initial_value(false)
                .interact()?
        {
            return Err(DialogError::Canceled);
        }
        let message = if !set.is_empty() {
            format!("Backing up {} to {remote}", count(set.len(), "ref"))
        } else if diff.diverged.is_empty() && diff.remote_only.is_empty() {
            format!("Everything up to date on {remote}")
        } else {
            format!("Nothing to send to {remote}")
        };
        planned = Some((diff, set));
        Ok(message.into())
    })
    .await?;
    let Some((diff, set)) = planned else {
        return Ok(());
    };

    if !set.is_empty() {
        let approval = remotes
            .approval_digest(&set.names())
            .context("gage backup")?;
        let status = remotes
            .push(&repository, &set, &approval)
            .context("gage backup")?;
        if !status.success() {
            return Err(CliError::message(format!(
                "gage backup: backup to {remote} was rejected"
            )));
        }
        let sent = set.send.len() + set.force.len();
        if set.delete.is_empty() {
            println!("Backed up {} to {remote}", count(sent, "ref"));
        } else {
            println!(
                "Backed up {} to {remote}; deleted {}",
                count(sent, "ref"),
                count(set.delete.len(), "ref")
            );
        }
    }
    if !args.prune && !diff.remote_only.is_empty() {
        println!(
            "{} on {remote} not in store; delete them: gage backup --prune {remote}",
            count(diff.remote_only.len(), "ref")
        );
    }
    if !args.force && !diff.diverged.is_empty() {
        return Err(CliError::message(format!(
            "gage backup: {} on {remote} diverge from store and were not sent; replace them: gage backup --force {remote}",
            count(diff.diverged.len(), "ref")
        )));
    }
    Ok(())
}

/// What the push changes on the remote: new and updated refs always,
/// diverged refs when `force`, remote-only refs when `prune`.
fn push_set(diff: &RemoteDiff, force: bool, prune: bool) -> PushSet {
    let mut send = diff.new.clone();
    send.extend(diff.update.iter().cloned());
    PushSet {
        send,
        force: if force {
            diff.diverged.clone()
        } else {
            Vec::new()
        },
        delete: if prune {
            diff.remote_only.clone()
        } else {
            Vec::new()
        },
    }
}

/// The preview: how many objects and tags are sent, how many the
/// remote already has, and what is left alone.
fn summary_lines(diff: &RemoteDiff, set: &PushSet) -> Vec<String> {
    let sent = set.send.iter().chain(&set.force);
    let (objects, tags) = sent.fold((0, 0), |(objects, tags), name| {
        if name.starts_with(TAG_REFS) {
            (objects, tags + 1)
        } else {
            (objects + 1, tags)
        }
    });
    let mut lines = vec![
        count(objects, "object"),
        count(tags, "tag"),
        format!("{} already on the remote", diff.same),
    ];
    if set.force.is_empty() && !diff.diverged.is_empty() {
        lines.push(format!("{} diverged", count(diff.diverged.len(), "ref")));
    }
    if set.delete.is_empty() && !diff.remote_only.is_empty() {
        lines.push(format!(
            "{} only on the remote",
            count(diff.remote_only.len(), "ref")
        ));
    }
    lines
}

/// One line per local ref: `<type> <short id>` for an object, read
/// from the commit the store holds, and `tag <name>` for a tag.
fn local_ref_lines(store: &Store, refs: &[String]) -> Result<Vec<String>, StoreError> {
    let mut lines = Vec::with_capacity(refs.len());
    for name in refs {
        let Some(id) = name.strip_prefix(OBJECT_REFS) else {
            lines.push(remote_ref_line(name));
            continue;
        };
        let sha = store
            .rev_parse(name)?
            .ok_or_else(|| StoreError::MissingObject(name.clone()))?;
        let header = store.read_header(&sha)?;
        lines.push(format!(
            "{} {}",
            type_name(&header.object_type),
            short_uuid(id)
        ));
    }
    Ok(lines)
}

/// One line for a ref the store has no commit for: `object <short id>`
/// or `tag <name>`.
fn remote_ref_line(name: &str) -> String {
    if let Some(tag) = name.strip_prefix(TAG_REFS) {
        format!("tag {tag}")
    } else if let Some(id) = name.strip_prefix(OBJECT_REFS) {
        format!("object {}", short_uuid(id))
    } else {
        name.to_string()
    }
}

/// `gage::note` displays as `note`
fn type_name(object_type: &str) -> &str {
    object_type.strip_prefix("gage::").unwrap_or(object_type)
}

/// The first `LIST_CAP` lines, then one line counting the rest.
fn capped(lines: Vec<String>) -> Vec<String> {
    if lines.len() <= LIST_CAP {
        return lines;
    }
    let rest = lines.len() - LIST_CAP;
    let mut shown: Vec<String> = lines.into_iter().take(LIST_CAP).collect();
    shown.push(format!("and {rest} more"));
    shown
}

/// What a push that replaces or deletes on the remote loses, one line
/// per effect in force.
fn effect_lines(force: usize, delete: usize, remote: &str) -> Vec<String> {
    let mut lines = Vec::new();
    if force > 0 {
        lines.push(format!(
            "Replacing {} on {remote} discards the remote's copy of each; content the store does not have is lost",
            count(force, "ref")
        ));
    }
    if delete > 0 {
        lines.push(format!(
            "Deleting {} from {remote} removes objects and tags the store does not have; nothing restores them",
            count(delete, "ref")
        ));
    }
    lines
}

/// The question before a push that replaces or deletes on the remote.
fn replace_prompt(force: usize, delete: usize, remote: &str) -> String {
    match (force, delete) {
        (n, 0) => format!("Replace {} on {remote}?", count(n, "ref")),
        (0, m) => format!("Delete {} from {remote}?", count(m, "ref")),
        (n, m) => format!("Replace {n} and delete {m} refs on {remote}?"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff() -> RemoteDiff {
        RemoteDiff {
            new: vec![format!("{OBJECT_REFS}new")],
            update: vec![format!("{TAG_REFS}v1")],
            diverged: vec![format!("{OBJECT_REFS}div")],
            remote_only: vec![format!("{OBJECT_REFS}only"), format!("{TAG_REFS}old")],
            same: 4,
        }
    }

    #[test]
    fn push_set_sends_new_and_updated_and_gates_the_rest_on_the_flags() {
        let diff = diff();
        let plain = push_set(&diff, false, false);
        assert_eq!(
            plain.send,
            vec![format!("{OBJECT_REFS}new"), format!("{TAG_REFS}v1")]
        );
        assert!(plain.force.is_empty());
        assert!(plain.delete.is_empty());
        assert_eq!(
            summary_lines(&diff, &plain),
            vec![
                "1 object",
                "1 tag",
                "4 already on the remote",
                "1 ref diverged",
                "2 refs only on the remote",
            ]
        );

        let all = push_set(&diff, true, true);
        assert_eq!(all.force, diff.diverged);
        assert_eq!(all.delete, diff.remote_only);
        assert_eq!(all.len(), 5);
        assert_eq!(
            summary_lines(&diff, &all),
            vec!["2 objects", "1 tag", "4 already on the remote"]
        );
    }

    #[test]
    fn ref_lines_name_tags_and_objects_and_the_list_is_capped() {
        assert_eq!(remote_ref_line(&format!("{TAG_REFS}s/x")), "tag s/x");
        assert_eq!(
            remote_ref_line(&format!("{OBJECT_REFS}63fpga15vgb58snz3y8s953vys")),
            "object 63fpga15"
        );
        let lines: Vec<String> = (0..LIST_CAP + 3).map(|i| i.to_string()).collect();
        let shown = capped(lines.clone());
        assert_eq!(shown.len(), LIST_CAP + 1);
        assert_eq!(shown[LIST_CAP], "and 3 more");
        assert_eq!(capped(lines[..LIST_CAP].to_vec()).len(), LIST_CAP);
    }

    #[test]
    fn effect_lines_name_what_is_lost() {
        assert!(effect_lines(0, 0, "github").is_empty());
        assert_eq!(
            effect_lines(1, 0, "github"),
            vec![
                "Replacing 1 ref on github discards the remote's copy of each; content the store does not have is lost"
            ]
        );
        assert_eq!(
            effect_lines(0, 2, "github"),
            vec![
                "Deleting 2 refs from github removes objects and tags the store does not have; nothing restores them"
            ]
        );
        assert_eq!(effect_lines(3, 2, "github").len(), 2);
    }

    #[test]
    fn replace_prompt_names_what_changes() {
        assert_eq!(replace_prompt(1, 0, "github"), "Replace 1 ref on github?");
        assert_eq!(replace_prompt(0, 2, "github"), "Delete 2 refs from github?");
        assert_eq!(
            replace_prompt(3, 2, "github"),
            "Replace 3 and delete 2 refs on github?"
        );
    }
}
