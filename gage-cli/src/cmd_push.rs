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
//! nothing local either way.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use clap::Args;
use cliclack as cli;
use datafusion::arrow::array::StringArray;
use gage_query::ContextBuilder;
use gage_store::{
    IdMatch, OBJECT_REFS, RemoteStore, Store, StoreError, TAG_REFS, TagStore, object_key_ref,
};

use crate::cmd_session::run_query;
use crate::dialog::{self, DialogError};

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
    /// notes, issues, and agent sessions
    #[arg(required = true)]
    objects: Vec<String>,

    /// Also push notes targeting named objects
    #[arg(long)]
    with_notes: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

/// One ref to send: its name and the local commit it points at.
struct RefToSend {
    name: String,
    sha: String,
}

pub async fn main(args: PushArgs) {
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => fail(&e),
    };
    let repository = match RemoteStore::from(&store).resolve(&args.repository) {
        Ok(repository) => repository,
        Err(e) => fail(&e),
    };
    let remote = repository.name();

    // Resolve every argument before anything else, so one bad
    // argument leaves nothing half done
    let mut tag_refs: BTreeSet<String> = BTreeSet::new();
    let mut ids: Vec<String> = Vec::with_capacity(args.objects.len());
    for object in &args.objects {
        match TagStore::from(&store).resolve(object) {
            Ok(Some(_)) => {
                tag_refs.insert(format!("{TAG_REFS}{object}"));
            }
            Ok(None) => {}
            Err(e) => fail(&e),
        }
        match store.resolve_in(object, None) {
            Ok(found) => ids.push(found.id),
            Err(e) => {
                eprintln!("gage push: {object}: {e}");
                std::process::exit(1);
            }
        }
    }

    let store = Arc::new(Mutex::new(store));
    let mut objects = closure(&store, &ids);
    if args.with_notes {
        let targets: Vec<String> = objects.iter().map(|m| m.id.clone()).collect();
        let notes = notes_targeting(&store, &targets).await;
        if !notes.is_empty() {
            let mut all: Vec<String> = objects.iter().map(|m| m.id.clone()).collect();
            all.extend(notes);
            objects = closure(&store, &all);
        }
    }

    let refs = match refs_to_send(&store, &objects, &tag_refs) {
        Ok(refs) => refs,
        Err(e) => fail(&e),
    };
    let on_remote = {
        let store = store.lock().unwrap();
        match RemoteStore::from(&*store).ls_remote(&repository) {
            Ok(on_remote) => on_remote,
            Err(e) => fail(&e),
        }
    };
    let (send, up_to_date): (Vec<&RefToSend>, Vec<&RefToSend>) = refs
        .iter()
        .partition(|r| on_remote.get(&r.name) != Some(&r.sha));
    if send.is_empty() {
        dialog::run(&format!("Push to {remote}"), || {
            Ok(format!("Everything up to date on {remote}").into())
        });
        return;
    }
    let send_names: Vec<String> = send.iter().map(|r| r.name.clone()).collect();

    let summary = summary_lines(&objects, &tag_refs, &refs, up_to_date.len());
    // The dialog previews and confirms; the push runs after it, so
    // git's own output lands outside the dialog
    let mut confirmed = false;
    dialog::run(&format!("Push to {remote}"), || {
        cli::log::remark(summary.join("\n"))?;
        if !args.yes && !cli::confirm("Continue?").initial_value(false).interact()? {
            return Err(DialogError::Canceled);
        }
        confirmed = true;
        Ok(format!("Pushing {} refs to {remote}", send_names.len()).into())
    });
    if !confirmed {
        return;
    }

    let store = store.lock().unwrap();
    let remotes = RemoteStore::from(&*store);
    let approval = match remotes.approval_digest(&send_names) {
        Ok(approval) => approval,
        Err(e) => fail(&e),
    };
    let status = match remotes.push(&repository, &send_names, &approval) {
        Ok(status) => status,
        Err(e) => fail(&e),
    };
    if !status.success() {
        fail(&format!(
            "push to {remote} was rejected; fetch and retry: gage fetch {remote}"
        ));
    }
    println!("Pushed {} refs to {remote}", send_names.len());
}

/// The named objects and everything they link, transitively.
fn closure(store: &Arc<Mutex<Store>>, ids: &[String]) -> Vec<IdMatch> {
    let store = store.lock().unwrap();
    match store.link_closure(ids) {
        Ok(objects) => objects,
        Err(e) => fail(&e),
    }
}

/// Ids of the live notes whose target is one of `targets`. The index
/// holds the reverse direction of a note's target link, which the
/// objects themselves do not.
async fn notes_targeting(store: &Arc<Mutex<Store>>, targets: &[String]) -> Vec<String> {
    let ctx = ContextBuilder::new(Some(Arc::clone(store))).build().await;
    let in_list = targets
        .iter()
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql =
        format!("SELECT DISTINCT note_id FROM note_target_link WHERE target_id IN ({in_list})");
    let mut notes = Vec::new();
    for batch in run_query(&ctx, &sql).await {
        let column = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("note_id is a string column");
        notes.extend(column.iter().flatten().map(String::from));
    }
    notes
}

/// Every ref the push sends: one object ref per object, the key ref of
/// each live object carrying a key, and the tag refs named on the
/// command line, each with the commit it points at locally.
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
        if object.deleted {
            continue;
        }
        if let Some(key_ref) = object_key_ref(&store.read_object(&object.tip_sha)?) {
            refs.push(RefToSend {
                name: key_ref,
                sha: object.tip_sha.clone(),
            });
        }
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

/// The preview: a count per object type, then the tags and keys, then
/// how many refs the remote already has.
fn summary_lines(
    objects: &[IdMatch],
    tag_refs: &BTreeSet<String>,
    refs: &[RefToSend],
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
    let keys = refs.len() - objects.len() - tag_refs.len();
    if keys > 0 {
        lines.push(format!("{keys} key{}", if keys == 1 { "" } else { "s" }));
    }
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

fn fail(e: &dyn std::fmt::Display) -> ! {
    eprintln!("gage push: {e}");
    std::process::exit(1)
}
