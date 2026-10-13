//! `gage fetch`: adopt what a remote has.
//!
//! The source is a configured remote or a bare Git URL or path. With
//! no objects named, every Gage ref the source advertises is fetched
//! into its tracking refs and decided; with objects named, those
//! objects and everything they link. A divergence the type can merge
//! is merged; one it cannot is reported as a conflict and left, and
//! `--ours` or `--theirs` on a later run of the named objects resolves
//! it.
//!
//! A URL is fetched into a transient namespace that is deleted before
//! the command exits, so the store keeps no record of the URL. The
//! namespace is deleted on the error path too; a run that ends another
//! way leaves one behind, and the next URL fetch clears it.

use std::collections::{BTreeMap, HashSet};

use clap::Args;
use cliclack as cli;
use gage_core::uuid::short_uuid;
use gage_store::{
    Conflict, FetchReport, FetchSource, KEY_REFS, OBJECT_REFS, Outcome, RemoteStore, Side, Store,
    StoreError, TAG_REFS,
};

use crate::author::resolve_author;
use crate::dialog::{self, DialogError, DialogResult};
use crate::human::count;

#[derive(Args)]
pub struct FetchArgs {
    /// Remote or URL to fetch from
    ///
    /// A remote name from `gage remote add`, or any Git URL or path.
    /// A URL is fetched without adding a remote, and nothing of it
    /// stays in the store
    repository: String,

    /// Objects to fetch (tag, ID, or prefix)
    ///
    /// Tag names, object IDs, or unique ID prefixes as the remote
    /// has them. Everything an object links is fetched with it. With
    /// none, pass --all to fetch the whole remote
    objects: Vec<String>,

    /// Fetch every object and tag from remote
    ///
    /// Required when no object is named. The fetch still previews the
    /// counts and asks to continue
    #[arg(long, conflicts_with = "objects")]
    all: bool,

    /// Keep local side on conflict
    #[arg(long, requires = "objects", conflicts_with = "theirs")]
    ours: bool,

    /// Take remote side on conflict
    #[arg(long, requires = "objects")]
    theirs: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub fn main(args: FetchArgs) {
    if args.objects.is_empty() && !args.all {
        fail(&"name objects to fetch, or pass --all to fetch the whole remote");
    }
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => fail(&e),
    };
    let remotes = RemoteStore::from(&store);
    let source = match remotes.resolve(&args.repository) {
        Ok(repository) => FetchSource::from(repository),
        Err(e) => fail(&e),
    };
    let choose = match (args.ours, args.theirs) {
        (true, _) => Some(Side::Ours),
        (_, true) => Some(Side::Theirs),
        _ => None,
    };

    if source.is_transient()
        && let Err(e) = remotes.clear_transient()
    {
        fail(&e);
    }
    let remote = source.repository().name().to_string();
    // The dialog previews and confirms; the fetch runs after it, so
    // git's own output lands outside the dialog
    let mut plan: Option<FetchPlan> = None;
    dialog::run(&format!("Fetch from {remote}"), || {
        fetch_dialog(&remotes, &source, &args, &remote, &mut plan)
    });
    let Some(plan) = plan else {
        return;
    };

    let result = fetch(
        &store,
        &source,
        plan.named.as_deref(),
        &plan.advertised,
        choose,
    );
    let discarded = remotes.discard(&source);
    let report = match result {
        Ok(report) => report,
        Err(message) => {
            for line in message.lines() {
                eprintln!("gage fetch: {line}");
            }
            if let Err(e) = discarded {
                eprintln!("gage fetch: {e}");
            }
            std::process::exit(1);
        }
    };
    print_report(&remote, &report);
    if let Err(e) = discarded {
        fail(&e);
    }
    if report.conflicts().next().is_some() {
        std::process::exit(1);
    }
}

/// What the dialog settled: the remote's advertised refs, and the
/// named ones to fetch when objects were given
struct FetchPlan {
    advertised: BTreeMap<String, String>,
    named: Option<Vec<String>>,
}

/// Preview what the remote advertises and confirm.
fn fetch_dialog(
    remotes: &RemoteStore<'_>,
    source: &FetchSource,
    args: &FetchArgs,
    remote: &str,
    plan: &mut Option<FetchPlan>,
) -> Result<DialogResult, DialogError> {
    // A static line, not a spinner: an ssh passphrase prompt is
    // written at the cursor, and a spinner's redraw erases it
    cli::log::step(format!("Listing refs on {remote}"))?;
    let advertised = remotes
        .ls_remote(source.repository())
        .map_err(|e| DialogError::Failed(e.to_string()))?;
    let named: Option<Vec<String>> = if args.objects.is_empty() {
        let n = |prefix: &str| advertised.keys().filter(|r| r.starts_with(prefix)).count();
        cli::log::remark(format!(
            "{}, {}, {} on the remote",
            count(n(OBJECT_REFS), "object"),
            count(n(TAG_REFS), "tag"),
            count(n(KEY_REFS), "key")
        ))?;
        None
    } else {
        let named =
            resolve_remote_objects(&args.objects, &advertised).map_err(DialogError::Failed)?;
        dialog::step_list("Objects", named.iter().map(|r| ref_label(r)))?;
        Some(named)
    };

    if !args.yes && !cli::confirm("Continue?").initial_value(true).interact()? {
        return Err(DialogError::Canceled);
    }
    *plan = Some(FetchPlan { advertised, named });
    Ok(format!("Fetching from {remote}").into())
}

/// A remote ref as the preview names it: `tag <name>` or the
/// object's short id
fn ref_label(ref_name: &str) -> String {
    match ref_name.strip_prefix(TAG_REFS) {
        Some(tag) => format!("tag {tag}"),
        None => short_uuid(ref_name.strip_prefix(OBJECT_REFS).unwrap_or(ref_name)).to_string(),
    }
}

/// Fetch into the tracking prefix of `source` and decide the local
/// refs: everything the remote advertises, or `named` and what it
/// links. An error is the lines to show.
fn fetch(
    store: &Store,
    source: &FetchSource,
    named: Option<&[String]>,
    advertised: &BTreeMap<String, String>,
    choose: Option<Side>,
) -> Result<FetchReport, String> {
    let remotes = RemoteStore::from(store);
    let only: Option<HashSet<String>> = match named {
        None => {
            let status = remotes.fetch_all(source).map_err(|e| e.to_string())?;
            if !status.success() {
                return Err(format!("git fetch {} failed", source.repository().name()));
            }
            None
        }
        Some(named) => {
            Some(fetch_closure(store, source, named, advertised).map_err(|e| e.to_string())?)
        }
    };
    let author = resolve_author(None);
    store
        .apply_fetched(source, only.as_ref(), &author, choose)
        .map_err(|e| e.to_string())
}

/// Each argument against the remote's advertised refs: an exact tag
/// name, else an id or a prefix unique among the remote's objects.
/// Returns the real ref names. Every argument is resolved before
/// anything is fetched; the error carries one line per argument that
/// did not resolve.
fn resolve_remote_objects(
    objects: &[String],
    advertised: &BTreeMap<String, String>,
) -> Result<Vec<String>, String> {
    let mut refs = Vec::with_capacity(objects.len());
    let mut errors: Vec<String> = Vec::new();
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
            [] => errors.push(format!("{object}: not found on the remote")),
            many => {
                let ids: Vec<String> = many
                    .iter()
                    .filter_map(|r| r.strip_prefix(OBJECT_REFS))
                    .map(|id| short_uuid(id).to_string())
                    .collect();
                errors.push(format!(
                    "{object}: ambiguous on the remote ({})",
                    ids.join(", ")
                ));
            }
        }
    }
    if errors.is_empty() {
        Ok(refs)
    } else {
        Err(errors.join("\n"))
    }
}

/// Fetch `named` and, round by round, the object refs of everything
/// the fetched commits link, until nothing new is reached. Returns
/// every real ref name fetched.
fn fetch_closure(
    store: &Store,
    source: &FetchSource,
    named: &[String],
    advertised: &BTreeMap<String, String>,
) -> Result<HashSet<String>, StoreError> {
    let remotes = RemoteStore::from(store);
    let mut fetched: HashSet<String> = HashSet::new();
    let mut pending: Vec<String> = named.to_vec();
    while !pending.is_empty() {
        let status = remotes.fetch_refs(source, &pending)?;
        if !status.success() {
            return Err(StoreError::Parse(format!(
                "git fetch {} failed",
                source.repository().name()
            )));
        }
        let tracked = store.tracking_refs(source)?;
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
fn print_report(source: &str, report: &FetchReport) {
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
        println!("Nothing to fetch from {source}");
        return;
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(label, n)| format!("{n} {label}"))
        .collect();
    println!("Fetched from {source}: {}", parts.join(", "));
    if report.conflicts().next().is_some() {
        println!("Resolve a conflict with: gage fetch {source} <OBJECT>... --ours | --theirs");
    }
}

fn fail(e: &dyn std::fmt::Display) -> ! {
    eprintln!("gage fetch: {e}");
    std::process::exit(1)
}
