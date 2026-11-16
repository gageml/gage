//! `gage fetch`: bring what a remote has into its tracking refs.
//!
//! The repository is a configured remote. With objects named, those
//! objects and everything they link are fetched; with `--all`, every
//! Gage ref the remote advertises is. With neither, the objects are
//! the ones the remote records as its fetch default; a remote with
//! none needs one of the two, or `--list` to see what the remote has
//! without fetching.
//!
//! A fetch writes the remote's tracking refs and stops: no object of
//! the store changes, and `gage merge` is the step that changes them.
//! A URL has no tracking refs to keep, so fetching one is refused in
//! favor of `gage pull`, which fetches and merges in one run.
//!
//! The transfer is shared with `gage pull`: [`plan_transfer`] previews
//! and confirms inside the dialog, and [`transfer`] runs after it.

use std::collections::{BTreeMap, HashSet};

use anyhow::Context;
use clap::Args;
use cliclack as cli;
use gage_core::uuid::short_uuid;
use gage_store::{
    FetchSource, KEY_REFS, OBJECT_REFS, RemoteStore, Repository, Store, StoreError, TAG_REFS,
};

use tabled::{
    Table,
    settings::{
        Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::dialog::{self, DialogError, DialogResult};
use crate::error::CliError;
use crate::human::count;
use crate::style;

#[derive(Args)]
pub struct FetchArgs {
    /// Remote to fetch from
    ///
    /// A remote name from `gage remote add`. A Git URL or path is
    /// fetched and merged in one run by `gage pull`
    remote: String,

    /// Objects to fetch (tag, ID, or prefix)
    ///
    /// Tag names, object IDs, or unique ID prefixes as the remote
    /// has them. Everything an object links is fetched with it. With
    /// none, objects that remote records as its fetch default are
    /// fetched; pass --all to fetch the whole remote
    objects: Vec<String>,

    /// Fetch every object and tag from remote
    ///
    /// Required when no object is named and remote records no fetch
    /// default. The fetch still previews the
    /// counts and asks to continue
    #[arg(long, conflicts_with = "objects")]
    all: bool,

    /// List what remote has without fetching
    ///
    /// Prints every tag the remote advertises with how the local
    /// store compares: new, same, or differs. Objects follow as a
    /// count by state. Transfers nothing and writes nothing
    #[arg(long, conflicts_with_all = ["objects", "all", "yes"])]
    list: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub fn main(args: FetchArgs) -> Result<(), CliError> {
    let store = Store::open(&gage_store::store_path()).context("gage fetch")?;
    let remotes = RemoteStore::from(&store);
    let repository = remotes.resolve(&args.remote).context("gage fetch")?;
    let remote = repository.name().to_string();
    if args.list {
        return list_remote(&remotes, &repository, &store, &remote)
            .context("gage fetch")
            .map_err(CliError::from);
    }
    require_remote(&repository).context("gage fetch")?;
    let objects = selected_objects(&remotes, &repository, args.objects, args.all, "fetch")
        .context("gage fetch")?;
    let source = FetchSource::from(repository);

    // The dialog previews and confirms; the fetch runs after it, so
    // git's own output lands outside the dialog
    let mut plan: Option<TransferPlan> = None;
    dialog::run(&format!("Fetch from {remote}"), || {
        plan_transfer(&remotes, &source, &objects, args.yes, &mut plan)
    })?;
    let Some(plan) = plan else {
        return Ok(());
    };
    let fetched = transfer(&store, &source, &plan)
        .map_err(|message| CliError::message(prefix_lines("gage fetch", &message)))?;
    let fetched = match fetched {
        Some(named) => named,
        None => store
            .tracking_refs(&source)
            .context("gage fetch")?
            .into_keys()
            .collect(),
    };
    println!("{}", fetched_line(&remote, &fetched));
    if !fetched.is_empty() {
        println!(
            "Merge into store with: {}",
            merge_command(&remote, &objects)
        );
    }
    Ok(())
}

/// A fetch keeps what it transfers under the tracking refs of a
/// remote. A URL or path has none, so it is refused.
fn require_remote(repository: &Repository) -> anyhow::Result<()> {
    if !repository.is_remote() {
        anyhow::bail!(
            "{} is not a remote; fetch and merge a URL or path in one run with gage pull, \
             or add it with gage remote add",
            repository.name()
        );
    }
    Ok(())
}

/// What a fetch, merge, or pull takes from a repository
pub enum Objects {
    /// Every object and tag
    All,
    /// The named objects, as given, and everything they link
    Named(Vec<String>),
}

/// The objects a run takes: every one under `--all`, the named ones,
/// or with neither the fetch default of `repository`. A URL records
/// no default, and a remote may record none; either is an error that
/// names `verb`, the run's own word for what it does.
pub fn selected_objects(
    remotes: &RemoteStore<'_>,
    repository: &Repository,
    objects: Vec<String>,
    all: bool,
    verb: &str,
) -> anyhow::Result<Objects> {
    if all {
        return Ok(Objects::All);
    }
    if !objects.is_empty() {
        return Ok(Objects::Named(objects));
    }
    let default = if repository.is_remote() {
        remotes.get(repository.name())?.fetch
    } else {
        Vec::new()
    };
    if default.is_empty() {
        anyhow::bail!(
            "name objects to {verb}, or pass --all to {verb} every object; \
             gage fetch --list shows what a remote has"
        );
    }
    Ok(Objects::Named(default))
}

/// Print what the remote advertises without fetching: its tags in
/// full, then a count of its objects by state. An object id carries
/// no description, so objects are a scale and drift signal, not a
/// listing. State is decided from the advertised commit alone, so a
/// local ref the remote's commit precedes reads `differs`, not
/// `behind`.
fn list_remote(
    remotes: &RemoteStore<'_>,
    repository: &Repository,
    store: &Store,
    remote: &str,
) -> Result<(), StoreError> {
    let advertised = remotes.ls_remote(repository)?;
    let mut local: BTreeMap<String, Option<String>> = BTreeMap::new();
    for name in advertised.keys() {
        local.insert(name.clone(), store.rev_parse(name)?);
    }
    let listing = classify_advertised(&advertised, &local);
    let objects = listing.objects();
    if listing.tags.is_empty() && objects == 0 {
        println!("Nothing on {remote}");
        return Ok(());
    }
    if !listing.tags.is_empty() {
        let header = vec!["Tag".to_string(), "State".to_string()];
        let table = Table::from_iter(std::iter::once(header).chain(listing.tags))
            .with(Style::rounded())
            .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
            .modify(
                Columns::one(0).not(Rows::first()),
                style::tty(Color::FG_BRIGHT_CYAN),
            )
            .to_string();
        println!("{table}");
    }
    if objects > 0 {
        let parts: Vec<String> = [
            ("new", listing.new),
            ("same", listing.same),
            ("differs", listing.differs),
        ]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(label, n)| format!("{n} {label}"))
        .collect();
        println!("{} ({})", count(objects, "object"), parts.join(", "));
    }
    Ok(())
}

/// The remote's advertised refs split against the local store: the
/// tag rows as `[tag, state]`, and the object counts by state.
struct RemoteListing {
    tags: Vec<Vec<String>>,
    new: usize,
    same: usize,
    differs: usize,
}

impl RemoteListing {
    fn objects(&self) -> usize {
        self.new + self.same + self.differs
    }
}

/// Decide each advertised ref's state from the local sha alone.
/// `new` is a ref the store does not have, `same` is a local commit
/// equal to the remote's, `differs` is any other local commit. Refs
/// that are neither tags nor objects are left out.
fn classify_advertised(
    advertised: &BTreeMap<String, String>,
    local: &BTreeMap<String, Option<String>>,
) -> RemoteListing {
    let mut tags: Vec<Vec<String>> = Vec::new();
    let (mut new, mut same, mut differs) = (0usize, 0usize, 0usize);
    for (name, remote_sha) in advertised {
        let state = match local.get(name).and_then(Option::as_deref) {
            None => "new",
            Some(local) if local == remote_sha.as_str() => "same",
            Some(_) => "differs",
        };
        if let Some(tag) = name.strip_prefix(TAG_REFS) {
            tags.push(vec![tag.to_string(), state.to_string()]);
        } else if name.starts_with(OBJECT_REFS) {
            match state {
                "new" => new += 1,
                "same" => same += 1,
                _ => differs += 1,
            }
        }
    }
    RemoteListing {
        tags,
        new,
        same,
        differs,
    }
}

/// What the dialog settled: the remote's advertised refs, and the
/// named ones to fetch when objects were given
pub struct TransferPlan {
    advertised: BTreeMap<String, String>,
    named: Option<Vec<String>>,
}

/// Preview what the remote advertises and confirm. Runs inside the
/// dialog of a fetch or a pull.
pub fn plan_transfer(
    remotes: &RemoteStore<'_>,
    source: &FetchSource,
    objects: &Objects,
    yes: bool,
    plan: &mut Option<TransferPlan>,
) -> Result<DialogResult, DialogError> {
    let remote = source.repository().name();
    // A static line, not a spinner: an ssh passphrase prompt is
    // written at the cursor, and a spinner's redraw erases it
    cli::log::step(format!("Listing refs on {remote}"))?;
    let advertised = remotes
        .ls_remote(source.repository())
        .map_err(|e| DialogError::Failed(e.to_string()))?;
    let named: Option<Vec<String>> = match objects {
        Objects::All => {
            cli::log::remark(format!("{} on the remote", ref_counts(advertised.keys())))?;
            None
        }
        Objects::Named(objects) => {
            let named = resolve_objects(objects, &advertised, "on the remote")
                .map_err(DialogError::Failed)?;
            dialog::step_list("Objects", named.iter().map(|r| ref_label(r)))?;
            Some(named)
        }
    };

    dialog::confirm(yes, true)?;
    *plan = Some(TransferPlan { advertised, named });
    Ok(format!("Fetching from {remote}").into())
}

/// The object, tag, and key refs among `refs`, as counts
pub fn ref_counts<'a>(refs: impl Iterator<Item = &'a String> + Clone) -> String {
    let n = |prefix: &str| refs.clone().filter(|r| r.starts_with(prefix)).count();
    format!(
        "{}, {}, {}",
        count(n(OBJECT_REFS), "object"),
        count(n(TAG_REFS), "tag"),
        count(n(KEY_REFS), "key")
    )
}

/// A ref as a preview names it: `tag <name>` or the object's short
/// id
pub fn ref_label(ref_name: &str) -> String {
    match ref_name.strip_prefix(TAG_REFS) {
        Some(tag) => format!("tag {tag}"),
        None => short_uuid(ref_name.strip_prefix(OBJECT_REFS).unwrap_or(ref_name)).to_string(),
    }
}

/// Fetch what `plan` settled into the tracking prefix of `source`:
/// everything the remote advertises, or the named refs and what they
/// link. Returns the real names of the refs a named fetch took, and
/// `None` for a fetch of everything. No local ref is written. An
/// error is the lines to show.
pub fn transfer(
    store: &Store,
    source: &FetchSource,
    plan: &TransferPlan,
) -> Result<Option<HashSet<String>>, String> {
    let remotes = RemoteStore::from(store);
    let Some(named) = &plan.named else {
        let status = remotes.fetch_all(source).map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("git fetch {} failed", source.repository().name()));
        }
        return Ok(None);
    };
    link_closure(store, source, named, &plan.advertised, |pending| {
        let status = remotes.fetch_refs(source, pending)?;
        if !status.success() {
            return Err(StoreError::Parse(format!(
                "git fetch {} failed",
                source.repository().name()
            )));
        }
        Ok(())
    })
    .map(Some)
    .map_err(|e| e.to_string())
}

/// Each argument against `refs`, the real names of the refs a
/// repository has: an exact tag name, else an id or a prefix unique
/// among the objects. Returns the real ref names. Every argument is
/// resolved before anything is taken; the error carries one line per
/// argument that did not resolve, and `place` says where it looked.
pub fn resolve_objects(
    objects: &[String],
    refs: &BTreeMap<String, String>,
    place: &str,
) -> Result<Vec<String>, String> {
    let mut resolved = Vec::with_capacity(objects.len());
    let mut errors: Vec<String> = Vec::new();
    for object in objects {
        let tag = format!("{TAG_REFS}{object}");
        if refs.contains_key(&tag) {
            resolved.push(tag);
            continue;
        }
        let matches: Vec<&String> = refs
            .keys()
            .filter(|r| {
                r.strip_prefix(OBJECT_REFS)
                    .is_some_and(|id| id.starts_with(object.as_str()))
            })
            .collect();
        match matches.as_slice() {
            [one] => resolved.push((*one).clone()),
            [] => errors.push(format!("{object}: not found {place}")),
            many => {
                let ids: Vec<String> = many
                    .iter()
                    .filter_map(|r| r.strip_prefix(OBJECT_REFS))
                    .map(|id| short_uuid(id).to_string())
                    .collect();
                errors.push(format!("{object}: ambiguous {place} ({})", ids.join(", ")));
            }
        }
    }
    if errors.is_empty() {
        Ok(resolved)
    } else {
        Err(errors.join("\n"))
    }
}

/// `named` and, round by round, the object refs of everything their
/// commits link, until nothing new is reached. `load` runs on each
/// round's refs before their tracking refs are read: a fetch
/// transfers them there, and a merge has them already. A linked
/// object is followed when `known` holds its ref. Returns every real
/// ref name reached.
pub fn link_closure(
    store: &Store,
    source: &FetchSource,
    named: &[String],
    known: &BTreeMap<String, String>,
    mut load: impl FnMut(&[String]) -> Result<(), StoreError>,
) -> Result<HashSet<String>, StoreError> {
    let mut reached: HashSet<String> = HashSet::new();
    let mut pending: Vec<String> = named.to_vec();
    while !pending.is_empty() {
        load(&pending)?;
        let tracked = store.tracking_refs(source)?;
        let mut next: Vec<String> = Vec::new();
        for ref_name in pending.drain(..) {
            reached.insert(ref_name.clone());
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
                if known.contains_key(&object_ref)
                    && !reached.contains(&object_ref)
                    && !next.contains(&object_ref)
                {
                    next.push(object_ref);
                }
            }
        }
        pending = next;
    }
    Ok(reached)
}

/// `message` with each line under the command's name.
pub fn prefix_lines(command: &str, message: &str) -> String {
    message
        .lines()
        .map(|line| format!("{command}: {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The line a fetch ends on: what its tracking refs took.
fn fetched_line(remote: &str, fetched: &HashSet<String>) -> String {
    let n = |prefix: &str| fetched.iter().filter(|r| r.starts_with(prefix)).count();
    if n(OBJECT_REFS) + n(TAG_REFS) == 0 {
        return format!("Nothing to fetch from {remote}");
    }
    format!(
        "Fetched from {remote}: {}, {}",
        count(n(OBJECT_REFS), "object"),
        count(n(TAG_REFS), "tag")
    )
}

/// The merge that takes what a fetch of `objects` brought.
fn merge_command(remote: &str, objects: &Objects) -> String {
    match objects {
        Objects::All => format!("gage merge {remote} --all"),
        Objects::Named(objects) => format!("gage merge {remote} {}", objects.join(" ")),
    }
}

#[cfg(test)]
pub mod tests {
    use std::path::Path;

    use gage_store::{NoteInput, NoteStore, NoteValue, TagStore};

    use super::*;

    /// A fresh store named `name` under `dir`
    pub fn open_store(dir: &Path, name: &str) -> Store {
        let path = dir.join(format!("{name}.git"));
        gage_store::init(&path).unwrap();
        Store::open_with_cache_root(&path, &dir.join(format!("{name}-cache"))).unwrap()
    }

    /// Two stores under one directory, `up` a remote of `down` by
    /// that name
    pub fn pair() -> (tempfile::TempDir, Store, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let up = open_store(tmp.path(), "up");
        let down = open_store(tmp.path(), "down");
        RemoteStore::from(&down)
            .add("up", up.path().to_str().unwrap(), false)
            .unwrap();
        (tmp, up, down)
    }

    pub fn note(store: &Store, name: &str) -> String {
        NoteStore::from(store)
            .create(NoteInput {
                name,
                value: NoteValue::Text("v1".into()),
                author: "user:test",
                target: None,
                metadata: None,
                work_id: None,
            })
            .unwrap()
    }

    pub fn object_ref(id: &str) -> String {
        format!("{OBJECT_REFS}{id}")
    }

    /// The plan the dialog settles for `objects`, without the dialog
    pub fn plan(store: &Store, source: &FetchSource, objects: &Objects) -> TransferPlan {
        let advertised = RemoteStore::from(store)
            .ls_remote(source.repository())
            .unwrap();
        let named = match objects {
            Objects::All => None,
            Objects::Named(objects) => {
                Some(resolve_objects(objects, &advertised, "on the remote").unwrap())
            }
        };
        TransferPlan { advertised, named }
    }

    /// With no object and no --all, a run takes the remote's fetch
    /// default as it was recorded. A remote with none and a URL have
    /// no default.
    #[test]
    fn a_run_with_no_object_takes_the_fetch_default_of_the_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path(), "store");
        let remotes = RemoteStore::from(&store);
        remotes.add("up", "/srv/up.git", false).unwrap();
        remotes.add("bare", "/srv/bare.git", false).unwrap();
        remotes
            .set_fetch("up", &["theirs".to_string(), "7zzz".to_string()])
            .unwrap();
        remotes.set_push("bare", &["work".to_string()]).unwrap();
        let objects = |repository: &str, all: bool| {
            let repository = remotes.resolve(repository).unwrap();
            selected_objects(&remotes, &repository, Vec::new(), all, "fetch")
        };

        match objects("up", false).unwrap() {
            Objects::Named(named) => assert_eq!(named, ["theirs", "7zzz"]),
            Objects::All => panic!("the default is named objects"),
        }
        assert!(matches!(objects("bare", true).unwrap(), Objects::All));
        for none in ["bare", "/srv/url.git"] {
            let err = objects(none, false).err().unwrap().to_string();
            assert!(err.contains("name objects to fetch"), "{none}: {err}");
        }
    }

    #[test]
    fn a_fetch_of_a_url_is_refused_and_names_pull() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path(), "store");
        let remotes = RemoteStore::from(&store);
        remotes.add("up", "/srv/up.git", false).unwrap();

        require_remote(&remotes.resolve("up").unwrap()).unwrap();
        let err = require_remote(&remotes.resolve("/srv/url.git").unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("/srv/url.git is not a remote"), "{err}");
        assert!(err.contains("gage pull"), "{err}");
    }

    /// A fetch of everything writes the tracking refs and no local
    /// ref, and the index gains no object.
    #[test]
    fn a_fetch_writes_tracking_refs_and_no_local_ref() {
        let (_tmp, up, down) = pair();
        let id = note(&up, "n");
        TagStore::from(&up).add("v1", &id, false).unwrap();
        let source = FetchSource::from(Repository::from_remote("up"));

        let fetched = transfer(&down, &source, &plan(&down, &source, &Objects::All)).unwrap();

        assert_eq!(fetched, None);
        let tracked = down.tracking_refs(&source).unwrap();
        assert_eq!(
            tracked.get(&object_ref(&id)),
            up.rev_parse(&object_ref(&id)).unwrap().as_ref()
        );
        assert!(tracked.contains_key(&format!("{TAG_REFS}v1")));
        assert_eq!(down.rev_parse(&object_ref(&id)).unwrap(), None);
        assert_eq!(down.rev_parse(&format!("{TAG_REFS}v1")).unwrap(), None);
        assert!(down.resolve_id(&id).is_err());
        assert_eq!(
            fetched_line("up", &tracked.into_keys().collect()),
            "Fetched from up: 1 object, 1 tag"
        );
    }

    /// A named fetch takes the named ref and what it links, and
    /// leaves the remote's other refs untracked.
    #[test]
    fn a_named_fetch_tracks_the_object_and_what_it_links() {
        let (_tmp, up, down) = pair();
        let id = note(&up, "n");
        let other = note(&up, "other");
        TagStore::from(&up).add("v1", &id, false).unwrap();
        let source = FetchSource::from(Repository::from_remote("up"));
        let objects = Objects::Named(vec!["v1".to_string()]);

        let fetched = transfer(&down, &source, &plan(&down, &source, &objects))
            .unwrap()
            .unwrap();

        let expected: HashSet<String> = [format!("{TAG_REFS}v1"), object_ref(&id)].into();
        assert_eq!(fetched, expected);
        let tracked = down.tracking_refs(&source).unwrap();
        assert!(tracked.contains_key(&object_ref(&id)));
        assert!(!tracked.contains_key(&object_ref(&other)));
        assert_eq!(down.rev_parse(&object_ref(&id)).unwrap(), None);
    }

    #[test]
    fn the_merge_hint_repeats_what_was_fetched() {
        assert_eq!(merge_command("up", &Objects::All), "gage merge up --all");
        assert_eq!(
            merge_command("up", &Objects::Named(vec!["v1".into(), "7zz".into()])),
            "gage merge up v1 7zz"
        );
        assert_eq!(
            fetched_line("up", &HashSet::new()),
            "Nothing to fetch from up"
        );
    }

    #[test]
    fn classify_advertised_splits_tags_and_counts_objects_by_state() {
        let advertised: BTreeMap<String, String> = [
            (format!("{OBJECT_REFS}new-obj"), "r1"),
            (format!("{OBJECT_REFS}same-obj"), "r2"),
            (format!("{OBJECT_REFS}diff-obj"), "r3"),
            (format!("{TAG_REFS}v1"), "r4"),
            (format!("{TAG_REFS}v2"), "r5"),
            (format!("{KEY_REFS}k1"), "r6"),
        ]
        .into_iter()
        .map(|(name, sha)| (name, sha.to_string()))
        .collect();
        let local: BTreeMap<String, Option<String>> = [
            (format!("{OBJECT_REFS}same-obj"), Some("r2".to_string())),
            (format!("{OBJECT_REFS}diff-obj"), Some("other".to_string())),
            (format!("{TAG_REFS}v2"), Some("r5".to_string())),
        ]
        .into_iter()
        .collect();

        let listing = classify_advertised(&advertised, &local);

        assert_eq!(listing.new, 1);
        assert_eq!(listing.same, 1);
        assert_eq!(listing.differs, 1);
        assert_eq!(listing.objects(), 3);
        assert_eq!(
            listing.tags,
            vec![
                vec!["v1".to_string(), "new".to_string()],
                vec!["v2".to_string(), "same".to_string()],
            ]
        );
    }
}
