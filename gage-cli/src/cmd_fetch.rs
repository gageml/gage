//! `gage fetch`: adopt what a remote has.
//!
//! With no objects named, every Gage ref the remote advertises is
//! fetched into its remote-tracking refs and decided; with objects
//! named, those objects and everything they link. A divergence the
//! type can merge is merged; one it cannot is reported as a conflict
//! and left, and `--ours` or `--theirs` on a later run of the named
//! objects resolves it.

use std::collections::{BTreeMap, HashSet};

use clap::Args;
use gage_core::uuid::short_uuid;
use gage_store::{
    Conflict, FetchReport, OBJECT_REFS, Outcome, RemoteStore, Side, Store, StoreError, TAG_REFS,
};

use crate::author::resolve_author;

#[derive(Args)]
pub struct FetchArgs {
    /// Remote to fetch from
    remote: String,

    /// Objects to fetch
    ///
    /// Tag names, object IDs, or unique ID prefixes as the remote
    /// has them. Everything an object links is fetched with it. With
    /// none, every object and tag the remote has
    objects: Vec<String>,

    /// Resolve conflicts on the named objects by keeping the local side
    #[arg(long, requires = "objects", conflicts_with = "theirs")]
    ours: bool,

    /// Resolve conflicts on the named objects by taking the remote side
    #[arg(long, requires = "objects")]
    theirs: bool,
}

pub fn main(args: FetchArgs) {
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => fail(&e),
    };
    let remote = args.remote.clone();
    let remotes = RemoteStore::from(&store);
    match remotes.list() {
        Ok(known) if known.iter().any(|r| r.name == remote) => {}
        Ok(_) => fail(&StoreError::RemoteNotFound(format!(
            "{remote} (add one with gage remote add)"
        ))),
        Err(e) => fail(&e),
    }
    let choose = match (args.ours, args.theirs) {
        (true, _) => Some(Side::Ours),
        (_, true) => Some(Side::Theirs),
        _ => None,
    };

    let only: Option<HashSet<String>> = if args.objects.is_empty() {
        match remotes.fetch_all(&remote) {
            Ok(status) if status.success() => {}
            Ok(_) => fail(&format!("git fetch {remote} failed")),
            Err(e) => fail(&e),
        }
        None
    } else {
        let advertised = match remotes.ls_remote(&remote) {
            Ok(advertised) => advertised,
            Err(e) => fail(&e),
        };
        let named = resolve_remote_objects(&args.objects, &advertised);
        match fetch_closure(&store, &remote, &named, &advertised) {
            Ok(fetched) => Some(fetched),
            Err(e) => fail(&e),
        }
    };

    let author = resolve_author(None);
    let report = match store.apply_fetched(&remote, only.as_ref(), &author, choose) {
        Ok(report) => report,
        Err(e) => fail(&e),
    };
    print_report(&remote, &report);
    if report.conflicts().next().is_some() {
        std::process::exit(1);
    }
}

/// Each argument against the remote's advertised refs: an exact tag
/// name, else an id or a prefix unique among the remote's objects.
/// Returns the real ref names. Every argument is resolved before
/// anything is fetched.
fn resolve_remote_objects(
    objects: &[String],
    advertised: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut refs = Vec::with_capacity(objects.len());
    let mut errors = 0;
    for object in objects {
        let tag = format!("{TAG_REFS}{object}");
        if advertised.contains_key(&tag) {
            refs.push(tag);
            continue;
        }
        let matches: Vec<&String> = advertised
            .keys()
            .filter(|r| {
                r.strip_prefix(OBJECT_REFS)
                    .is_some_and(|id| id.starts_with(object.as_str()))
            })
            .collect();
        match matches.as_slice() {
            [one] => refs.push((*one).clone()),
            [] => {
                eprintln!("gage fetch: {object}: not found on the remote");
                errors += 1;
            }
            many => {
                let ids: Vec<String> = many
                    .iter()
                    .filter_map(|r| r.strip_prefix(OBJECT_REFS))
                    .map(|id| short_uuid(id).to_string())
                    .collect();
                eprintln!(
                    "gage fetch: {object}: ambiguous on the remote ({})",
                    ids.join(", ")
                );
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }
    refs
}

/// Fetch `named` and, round by round, the object refs of everything
/// the fetched commits link, until nothing new is reached. Returns
/// every real ref name fetched.
fn fetch_closure(
    store: &Store,
    remote: &str,
    named: &[String],
    advertised: &BTreeMap<String, String>,
) -> Result<HashSet<String>, StoreError> {
    let remotes = RemoteStore::from(store);
    let mut fetched: HashSet<String> = HashSet::new();
    let mut pending: Vec<String> = named.to_vec();
    while !pending.is_empty() {
        let status = remotes.fetch_refs(remote, &pending)?;
        if !status.success() {
            return Err(StoreError::Parse(format!("git fetch {remote} failed")));
        }
        let tracked = store.tracking_refs(remote)?;
        let mut next: Vec<String> = Vec::new();
        for ref_name in pending.drain(..) {
            fetched.insert(ref_name.clone());
            let Some(sha) = tracked.get(&ref_name) else {
                continue;
            };
            // A tag's commit names its object; an object's commit
            // names what it links
            let mut shas = vec![sha.clone()];
            shas.extend(
                store
                    .find_link_files(sha)?
                    .into_iter()
                    .flat_map(|link| link.shas),
            );
            for sha in shas {
                let id = store.read_header(&sha)?.id;
                let object_ref = format!("{OBJECT_REFS}{id}");
                if advertised.contains_key(&object_ref)
                    && !fetched.contains(&object_ref)
                    && !next.contains(&object_ref)
                {
                    next.push(object_ref);
                }
            }
        }
        pending = next;
    }
    Ok(fetched)
}

/// One line per conflict, then a count per outcome.
fn print_report(remote: &str, report: &FetchReport) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for outcome in &report.outcomes {
        let label = match &outcome.outcome {
            Outcome::Created => "created",
            Outcome::UpToDate => "up to date",
            Outcome::Ahead => "ahead",
            Outcome::FastForwarded => "fast-forwarded",
            Outcome::Tombstoned => "deleted",
            Outcome::Resurrected => "restored",
            Outcome::Merged => "merged",
            Outcome::Resolved(_) => "resolved",
            Outcome::Conflict(conflict) => {
                let id = short_uuid(&outcome.id);
                let what = match conflict {
                    Conflict::Status { ours, theirs } => {
                        format!("status ours {ours}, theirs {theirs}")
                    }
                    Conflict::Object => "changed on both sides".to_string(),
                    Conflict::Member { id } => {
                        format!("member {} at divergent commits", short_uuid(id))
                    }
                    Conflict::Tag => "names a different object on each side".to_string(),
                    Conflict::Key { held_by } => {
                        format!("held by {}", short_uuid(held_by))
                    }
                };
                let subject = match outcome.kind.as_str() {
                    "tag" => format!(
                        "tag {}",
                        outcome
                            .ref_name
                            .strip_prefix(TAG_REFS)
                            .unwrap_or(&outcome.ref_name)
                    ),
                    "key" => format!("key {} ({})", outcome.ref_name, id),
                    kind => format!("{kind} {id}"),
                };
                println!("conflict: {subject}: {what}");
                "conflicts"
            }
        };
        *counts.entry(label).or_insert(0) += 1;
    }
    if counts.is_empty() {
        println!("Nothing to fetch from {remote}");
        return;
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(label, n)| format!("{n} {label}"))
        .collect();
    println!("Fetched from {remote}: {}", parts.join(", "));
    if report.conflicts().next().is_some() {
        println!("Resolve a conflict with: gage fetch {remote} <OBJECT>... --ours | --theirs");
    }
}

fn fail(e: &dyn std::fmt::Display) -> ! {
    eprintln!("gage fetch: {e}");
    std::process::exit(1)
}
