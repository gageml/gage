//! `gage push`: send objects to a remote.
//!
//! Each argument is an object-ish. The set sent is the named objects
//! and everything they link, transitively: a scan's dataset, the
//! dataset's sessions and attachments, the scan's notes, issues, and
//! agent sessions. Nothing the user did not name is sent, and nothing
//! named is sent without the preview and confirmation. The push
//! carries the approval digest the store's `pre-push` hook checks, so
//! the set that leaves is the set that was shown. The destination is
//! a configured remote or a bare Git URL or path; a push writes
//! nothing local either way. With no argument, the objects are the
//! ones the remote records as its push default.
//!
//! A retraction is sent in place of the commit the remote holds: a
//! tombstone, a published session written again without descent, and
//! a share rewritten without a removed member. Each takes content out
//! of reach, so none can be a fast-forward. The push replaces the
//! remote's commit under a lease on the commit the remote advertised.
//! A member a share dropped because it was removed is sent as its
//! tombstone with the share, so the remote stops holding its content.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::Args;
use cliclack as cli;
use datafusion::arrow::array::StringArray;
use gage_query::ContextBuilder;
use gage_store::{
    IdMatch, LeasedRef, OBJECT_REFS, PushSet, RemoteStore, Repository, Retraction, Store,
    StoreError, TAG_REFS, TagStore,
};

use crate::cmd_session::run_query;
use crate::dialog;
use crate::error::CliError;
use crate::human::count;

#[derive(Args)]
pub struct PushArgs {
    /// Remote or URL to push to
    ///
    /// A remote name from `gage remote add`, or any Git URL or path
    repository: String,

    /// Objects to push (tag, ID, or prefix)
    ///
    /// Tag names, object IDs, or unique ID prefixes. Everything an
    /// object links is pushed with it: a scan's dataset, sessions,
    /// notes, issues, and agent sessions. With none, objects that
    /// remote records as its push default are pushed
    objects: Vec<String>,

    /// Also push notes targeting named objects
    #[arg(long)]
    with_notes: bool,

    /// Replace remote commits this store has not fetched
    ///
    /// A deleted object, a republished session, and a share that lost
    /// a deleted member replace what a remote holds. When a remote
    /// holds a commit this store has not fetched, push stops; this
    /// option replaces that commit. Nothing else is replaced
    #[arg(long)]
    force: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

/// One ref to send: its name and the local commit it points at.
struct RefToSend {
    name: String,
    sha: String,
}

pub async fn main(args: PushArgs) -> Result<(), CliError> {
    let store = Store::open(&gage_store::store_path()).context("gage push")?;
    let repository = RemoteStore::from(&store)
        .resolve(&args.repository)
        .context("gage push")?;
    let remote = repository.name();
    let named = objects_to_push(&RemoteStore::from(&store), &repository, args.objects)
        .context("gage push")?;

    // Resolve every argument before anything else, so one bad
    // argument leaves nothing half done
    let mut tag_refs: BTreeSet<String> = BTreeSet::new();
    let mut ids: Vec<String> = Vec::with_capacity(named.len());
    for object in &named {
        if TagStore::from(&store)
            .resolve(object)
            .context("gage push")?
            .is_some()
        {
            tag_refs.insert(format!("{TAG_REFS}{object}"));
        }
        let found = store
            .resolve_in(object, None)
            .with_context(|| format!("gage push: {object}"))?;
        ids.push(found.id);
    }

    let store = Arc::new(Mutex::new(store));
    let mut objects = closure(&store, &ids).context("gage push")?;
    if args.with_notes {
        let targets: Vec<String> = objects.iter().map(|m| m.id.clone()).collect();
        let notes = notes_targeting(&store, &targets)
            .await
            .context("gage push")?;
        if !notes.is_empty() {
            let mut all: Vec<String> = objects.iter().map(|m| m.id.clone()).collect();
            all.extend(notes);
            objects = closure(&store, &all).context("gage push")?;
        }
    }

    let mut refs = refs_to_send(&store, &objects, &tag_refs).context("gage push")?;
    let (plan, on_remote) = {
        let store = store.lock().unwrap();
        let remotes = RemoteStore::from(&*store);
        let on_remote = remotes.ls_remote(&repository).context("gage push")?;
        let retracted = retracted_members(&store, &refs, &on_remote).context("gage push")?;
        refs.extend(retracted.iter().map(|m| RefToSend {
            name: format!("{OBJECT_REFS}{}", m.id),
            sha: m.tip_sha.clone(),
        }));
        objects.extend(retracted);
        let plan = plan_push(&remotes, &refs, &on_remote, args.force).context("gage push")?;
        (plan, on_remote)
    };
    if !plan.unverified.is_empty() {
        return Err(CliError::message(format!(
            "gage push: {remote} holds a commit of {} that this store has not fetched; \
             fetch first (gage fetch {remote}), or replace it with --force",
            count(plan.unverified.len(), "object")
        )));
    }
    let set = plan.set;
    if set.is_empty() {
        return dialog::run(&format!("Push to {remote}"), || {
            Ok(format!("Everything up to date on {remote}").into())
        });
    }
    let up_to_date = refs
        .iter()
        .filter(|r| on_remote.get(&r.name) == Some(&r.sha))
        .count();

    let mut summary = summary_lines(&objects, &tag_refs, up_to_date);
    if !set.replace.is_empty() {
        summary.push(format!(
            "Replacing {} on {remote}",
            count(set.replace.len(), "object")
        ));
    }
    // The dialog previews and confirms; the push runs after it, so
    // git's own output lands outside the dialog
    let mut confirmed = false;
    dialog::run(&format!("Push to {remote}"), || {
        cli::log::remark(summary.join("\n"))?;
        dialog::confirm(args.yes, false)?;
        confirmed = true;
        Ok(format!("Pushing {} refs to {remote}", set.len()).into())
    })?;
    if !confirmed {
        return Ok(());
    }

    let store = store.lock().unwrap();
    let remotes = RemoteStore::from(&*store);
    let approval = remotes.approval_digest(&set.names()).context("gage push")?;
    let status = remotes
        .push(&repository, &set, &approval)
        .context("gage push")?;
    if !status.success() {
        return Err(CliError::message(format!(
            "gage push: push to {remote} was rejected; fetch and retry: gage fetch {remote}"
        )));
    }
    println!("Pushed {} refs to {remote}", set.len());
    Ok(())
}

/// The objects a push names: those given, else the push default of
/// the remote. A URL records no default, and a remote may record
/// none; either is an error when no object is given.
fn objects_to_push(
    remotes: &RemoteStore<'_>,
    repository: &Repository,
    given: Vec<String>,
) -> anyhow::Result<Vec<String>> {
    if !given.is_empty() {
        return Ok(given);
    }
    if !repository.is_remote() {
        anyhow::bail!("name objects to push; only a remote records default objects");
    }
    let name = repository.name();
    let default = remotes.get(name)?.push;
    if default.is_empty() {
        anyhow::bail!(
            "name objects to push, or set default objects of {name}: \
             gage remote edit {name} --push <OBJECT>"
        );
    }
    Ok(default)
}

/// The removed objects that a ref of `refs` linked in the commit the
/// remote holds: a share's members removed since the share was last
/// sent. Each is a tombstone the push carries with the share. A remote
/// commit the store does not hold contributes none.
fn retracted_members(
    store: &Store,
    refs: &[RefToSend],
    on_remote: &BTreeMap<String, String>,
) -> Result<Vec<IdMatch>, StoreError> {
    let mut out: Vec<IdMatch> = Vec::new();
    for r in refs {
        let Some(remote_sha) = on_remote.get(&r.name) else {
            continue;
        };
        if *remote_sha == r.sha || !r.name.starts_with(OBJECT_REFS) {
            continue;
        }
        if store
            .object_info(&format!("{remote_sha}^{{commit}}"))?
            .is_none()
        {
            continue;
        }
        for member in store.tombstoned_links(remote_sha)? {
            let name = format!("{OBJECT_REFS}{}", member.id);
            let known =
                refs.iter().any(|r| r.name == name) || out.iter().any(|m| m.id == member.id);
            if !known {
                out.push(member);
            }
        }
    }
    Ok(out)
}

/// What a push does with each ref, and the object refs it cannot
/// place.
#[derive(Debug, Default, PartialEq, Eq)]
struct PushPlan {
    set: PushSet,
    /// Refs whose local commit is a retraction of a remote commit the
    /// store does not hold
    unverified: Vec<String>,
}

/// Decide what the push does with each of `refs` given what the remote
/// advertises. A ref the remote lacks is sent, and one it holds at the
/// same commit is left out. An object ref whose local commit retracts
/// the remote's is replaced under a lease on the advertised commit.
/// When the store does not hold the remote's commit, the ref is
/// replaced only with `force` and is otherwise reported. Every other
/// ref is sent, and the remote refuses it unless it is a fast-forward.
fn plan_push(
    remotes: &RemoteStore<'_>,
    refs: &[RefToSend],
    on_remote: &BTreeMap<String, String>,
    force: bool,
) -> Result<PushPlan, StoreError> {
    let mut plan = PushPlan::default();
    for r in refs {
        let Some(remote_sha) = on_remote.get(&r.name) else {
            plan.set.send.push(r.name.clone());
            continue;
        };
        if *remote_sha == r.sha {
            continue;
        }
        let retraction = if r.name.starts_with(OBJECT_REFS) {
            remotes.retraction(&r.sha, remote_sha)?
        } else {
            Retraction::No
        };
        let leased = || LeasedRef {
            name: r.name.clone(),
            expect: remote_sha.clone(),
        };
        match retraction {
            Retraction::No => plan.set.send.push(r.name.clone()),
            Retraction::Verified => plan.set.replace.push(leased()),
            Retraction::Unverified if force => plan.set.replace.push(leased()),
            Retraction::Unverified => plan.unverified.push(r.name.clone()),
        }
    }
    Ok(plan)
}

/// The named objects and everything they link, transitively.
fn closure(store: &Arc<Mutex<Store>>, ids: &[String]) -> Result<Vec<IdMatch>, StoreError> {
    let store = store.lock().unwrap();
    store.link_closure(ids)
}

/// Ids of the live notes whose target is one of `targets`. The index
/// holds the reverse direction of a note's target link, which the
/// objects themselves do not.
async fn notes_targeting(
    store: &Arc<Mutex<Store>>,
    targets: &[String],
) -> datafusion::error::Result<Vec<String>> {
    let ctx = ContextBuilder::new(Some(Arc::clone(store))).build().await;
    let in_list = targets
        .iter()
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql =
        format!("SELECT DISTINCT note_id FROM note_target_link WHERE target_id IN ({in_list})");
    let mut notes = Vec::new();
    for batch in run_query(&ctx, &sql).await? {
        let column = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("note_id is a string column");
        notes.extend(column.iter().flatten().map(String::from));
    }
    Ok(notes)
}

/// Every ref the push sends: one object ref per object and the tag
/// refs named on the command line, each with the commit it points at
/// locally. Key refs are not sent: a key ref is this store's index of
/// which live object carries a key, and a receiving store derives its
/// own from `attrs.key` in the object tree.
fn refs_to_send(
    store: &Arc<Mutex<Store>>,
    objects: &[IdMatch],
    tag_refs: &BTreeSet<String>,
) -> Result<Vec<RefToSend>, StoreError> {
    let store = store.lock().unwrap();
    let mut refs = Vec::with_capacity(objects.len() + tag_refs.len());
    for object in objects {
        refs.push(RefToSend {
            name: format!("{OBJECT_REFS}{}", object.id),
            sha: object.tip_sha.clone(),
        });
    }
    for name in tag_refs {
        let sha = store
            .rev_parse(name)?
            .ok_or_else(|| StoreError::TagNotFound(name.clone()))?;
        refs.push(RefToSend {
            name: name.clone(),
            sha,
        });
    }
    Ok(refs)
}

/// The preview: a count per object type, then the tags, then how many
/// refs the remote already has.
fn summary_lines(
    objects: &[IdMatch],
    tag_refs: &BTreeSet<String>,
    up_to_date: usize,
) -> Vec<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for object in objects {
        let type_name = object
            .object_type
            .strip_prefix("gage::")
            .unwrap_or(&object.object_type);
        *counts.entry(type_name).or_insert(0) += 1;
    }
    let mut lines: Vec<String> = counts
        .iter()
        .map(|(type_name, n)| format!("{n} {type_name}{}", if *n == 1 { "" } else { "s" }))
        .collect();
    if !tag_refs.is_empty() {
        let names: Vec<&str> = tag_refs
            .iter()
            .map(|r| r.strip_prefix(TAG_REFS).unwrap_or(r))
            .collect();
        lines.push(format!("tags: {}", names.join(", ")));
    }
    if up_to_date > 0 {
        lines.push(format!("{up_to_date} already on the remote"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::slice;

    use gage_session::SummaryAttrs;
    use gage_store::{
        PublishAttrs, SessionAttrsRecord, SessionDirLayout, SessionStore, ShareAttrs, ShareStore,
        published_session_id,
    };

    use super::*;

    /// A fresh store and the guard keeping its directory alive
    fn open_store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open_with_cache_root(&path, &tmp.path().join("cache")).unwrap();
        (tmp, store)
    }

    /// A push with no object takes the remote's push default as it
    /// was recorded; objects given take its place; a remote with no
    /// default and a URL both need objects.
    #[test]
    fn a_push_with_no_object_takes_the_push_default_of_the_remote() {
        let (_tmp, store) = open_store();
        let remotes = RemoteStore::from(&store);
        remotes.add("up", "/srv/up.git", false).unwrap();
        remotes.add("bare", "/srv/bare.git", false).unwrap();
        remotes
            .set_push("up", &["work".to_string(), "7zzz".to_string()])
            .unwrap();
        remotes.set_fetch("bare", &["theirs".to_string()]).unwrap();
        let objects = |repository: &str, given: &[&str]| {
            objects_to_push(
                &remotes,
                &remotes.resolve(repository).unwrap(),
                given.iter().map(|o| o.to_string()).collect(),
            )
        };

        assert_eq!(objects("up", &[]).unwrap(), ["work", "7zzz"]);
        assert_eq!(objects("up", &["other"]).unwrap(), ["other"]);
        assert_eq!(objects("/srv/url.git", &["other"]).unwrap(), ["other"]);

        let err = objects("bare", &[]).unwrap_err().to_string();
        assert!(err.contains("gage remote edit bare --push"), "{err}");
        let err = objects("/srv/url.git", &[]).unwrap_err().to_string();
        assert!(
            err.contains("only a remote records default objects"),
            "{err}"
        );
    }

    /// Write a published session directory for `id` under `root` with
    /// `content` as its one file.
    fn published_dir(root: &Path, id: &str, content: &str) -> PathBuf {
        let dir = root.join(id);
        let layout = SessionDirLayout::new(&dir);
        fs::create_dir_all(layout.files_dir()).unwrap();
        let attrs = SessionAttrsRecord {
            driver: "claude 1".into(),
            origin: "session:original@abc".into(),
            native_mtime: 0,
            native_size: 0,
            session_type: "claude".into(),
            content_format: "claude-jsonl 1".into(),
            size: 0,
            project: None,
            summary: SummaryAttrs::default(),
            summary_version: None,
            publish: Some(PublishAttrs::default()),
        };
        fs::write(layout.attrs_file(), serde_json::to_vec(&attrs).unwrap()).unwrap();
        fs::write(layout.changes_file(), "[]\n").unwrap();
        fs::write(layout.files_dir().join("session.jsonl"), content).unwrap();
        dir
    }

    /// A ref the remote lacks is sent and one it holds at the same
    /// commit is left out. A republished session, a removed session,
    /// and a share that lost the removed member replace the commits
    /// the remote holds, each under a lease on that commit, and the
    /// removed member's tombstone joins the push. An update that
    /// descends from the remote's commit is sent. A retraction of a
    /// commit the store does not hold is reported, and replaced only
    /// with force.
    #[test]
    fn a_push_replaces_what_a_retraction_supersedes_under_a_lease() {
        let (tmp, store) = open_store();
        let sessions = SessionStore::from(&store);
        let remotes = RemoteStore::from(&store);
        let tip = |id: &str| store.resolve_id(id).unwrap().1;
        let name = |id: &str| format!("{OBJECT_REFS}{id}");
        let to_send = |ids: &[&String]| -> Vec<RefToSend> {
            ids.iter()
                .map(|id| RefToSend {
                    name: name(id),
                    sha: tip(id),
                })
                .collect()
        };
        let ids: Vec<String> = ["republished", "removed", "updated", "fresh"]
            .iter()
            .map(|spec| published_session_id("original", spec))
            .collect();
        let [republished, removed, updated, fresh] = &ids[..] else {
            panic!("four ids");
        };
        for id in &ids {
            sessions
                .create_from_dir(&published_dir(tmp.path(), id, "a\nsecret\n"))
                .unwrap();
        }
        let share = ShareStore::from(&store)
            .create(&ShareAttrs::default(), &ids)
            .unwrap();
        let mut on_remote: BTreeMap<String, String> = [republished, removed, updated, &share]
            .iter()
            .map(|id| (name(id), tip(id)))
            .collect();

        sessions
            .replace_from_dir(&published_dir(tmp.path(), republished, "a\n"))
            .unwrap();
        sessions.remove(slice::from_ref(removed)).unwrap();
        sessions
            .update_from_dir(&published_dir(tmp.path(), updated, "a\nsecret\nb\n"))
            .unwrap();

        // An edit after the rewrite leaves the share's tip with a
        // parent; its history is still cut from the remote's commit
        let shares = ShareStore::from(&store);
        shares.unlink(&share, slice::from_ref(fresh)).unwrap();
        shares.link(&share, slice::from_ref(fresh)).unwrap();

        let mut refs = to_send(&[republished, updated, fresh, &share]);
        let retracted = retracted_members(&store, &refs, &on_remote).unwrap();
        assert_eq!(
            retracted.iter().map(|m| &m.id).collect::<Vec<_>>(),
            [removed]
        );
        refs.extend(to_send(&[removed]));
        let before = on_remote.clone();
        let leased = |id: &String| LeasedRef {
            name: name(id),
            expect: before[&name(id)].clone(),
        };
        let plan = plan_push(&remotes, &refs, &on_remote, false).unwrap();
        assert_eq!(plan.set.send, [name(updated), name(fresh)]);
        assert_eq!(
            plan.set.replace,
            [leased(republished), leased(&share), leased(removed)]
        );
        assert!(plan.unverified.is_empty());

        on_remote.insert(name(fresh), tip(fresh));
        on_remote.insert(name(removed), "0".repeat(40));
        let plan = plan_push(&remotes, &refs, &on_remote, false).unwrap();
        assert_eq!(plan.unverified, [name(removed)]);
        assert!(!plan.set.names().contains(&name(fresh)));
        let plan = plan_push(&remotes, &refs, &on_remote, true).unwrap();
        assert!(plan.unverified.is_empty());
        assert!(plan.set.replace.contains(&LeasedRef {
            name: name(removed),
            expect: "0".repeat(40),
        }));
    }
}
