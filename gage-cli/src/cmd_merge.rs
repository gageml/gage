//! `gage merge`: bring what a fetch tracked into the store's objects.
//!
//! The repository is a configured remote that `gage fetch` has
//! fetched. With objects named, those objects and everything they
//! link are merged; with `--all`, every fetched object and tag is.
//! With neither, the objects are the ones the remote records as its
//! fetch default. The objects are resolved against the fetched refs,
//! and the remote is never contacted.
//!
//! A divergence the type can merge is merged; one it cannot is
//! reported as a conflict and left, and `--ours` or `--theirs` on a
//! later run of the named objects resolves it.
//!
//! The merge and its report are shared with `gage pull`: [`merge`]
//! decides the local refs and [`print_report`] prints the outcomes.

use std::collections::{BTreeMap, HashSet};

use anyhow::Context;
use clap::Args;
use cliclack as cli;
use gage_core::uuid::short_uuid;
use gage_store::{
    Conflict, FetchSource, MergeReport, Outcome, RemoteStore, Repository, Side, Store, TAG_REFS,
};

use crate::author::resolve_author;
use crate::cmd_fetch::{
    Objects, link_closure, prefix_lines, ref_counts, ref_label, resolve_objects, selected_objects,
};
use crate::dialog::{self, DialogError, DialogResult};
use crate::error::CliError;

#[derive(Args)]
pub struct MergeArgs {
    /// Remote to merge from
    ///
    /// A remote name from `gage remote add`, as `gage fetch` fetched
    /// it. Remote is not contacted
    remote: String,

    /// Objects to merge (tag, ID, or prefix)
    ///
    /// Tag names, object IDs, or unique ID prefixes as the fetch
    /// of remote has them. Everything an object links is merged with
    /// it. With none, objects that remote records as its fetch
    /// default are merged; pass --all to merge everything fetched
    objects: Vec<String>,

    /// Merge every fetched object and tag
    ///
    /// Required when no object is named and remote records no fetch
    /// default. The merge still previews the
    /// counts and asks to continue
    #[arg(long, conflicts_with = "objects")]
    all: bool,

    #[command(flatten)]
    resolve: ResolveArgs,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

/// How a merge settles a divergence it cannot merge
#[derive(Args)]
pub struct ResolveArgs {
    /// Keep local side on conflict
    #[arg(long, requires = "objects", conflicts_with = "theirs")]
    ours: bool,

    /// Take remote side on conflict
    #[arg(long, requires = "objects")]
    theirs: bool,
}

impl ResolveArgs {
    /// The side the options name, when either is given.
    pub fn side(&self) -> Option<Side> {
        match (self.ours, self.theirs) {
            (true, _) => Some(Side::Ours),
            (_, true) => Some(Side::Theirs),
            _ => None,
        }
    }
}

pub fn main(args: MergeArgs) -> Result<(), CliError> {
    let store = Store::open(&gage_store::store_path()).context("gage merge")?;
    let remotes = RemoteStore::from(&store);
    let repository = remotes.resolve(&args.remote).context("gage merge")?;
    let source = FetchSource::from(repository);
    let tracked = fetched_refs(&store, &source).context("gage merge")?;
    let objects = selected_objects(
        &remotes,
        source.repository(),
        args.objects,
        args.all,
        "merge",
    )
    .context("gage merge")?;
    let remote = source.repository().name().to_string();

    let mut only: Option<Option<HashSet<String>>> = None;
    dialog::run(&format!("Merge from {remote}"), || {
        merge_dialog(&store, &source, &tracked, &objects, args.yes, &mut only)
    })?;
    let Some(only) = only else {
        return Ok(());
    };
    let report = merge(&store, &source, only.as_ref(), args.resolve.side())
        .map_err(|message| CliError::message(prefix_lines("gage merge", &message)))?;
    print_report(Run::Merge, &remote, &report);
    if report.conflicts().next().is_some() {
        return Err(CliError::exit(1));
    }
    Ok(())
}

/// The refs a fetch of `source` tracked, keyed by the real ref name.
/// A merge reads nothing else of the remote, so a URL, which keeps no
/// tracking refs, and a remote never fetched are errors.
fn fetched_refs(store: &Store, source: &FetchSource) -> anyhow::Result<BTreeMap<String, String>> {
    let repository: &Repository = source.repository();
    if !repository.is_remote() {
        anyhow::bail!(
            "{} is not a remote; fetch and merge a URL or path in one run with gage pull",
            repository.name()
        );
    }
    let tracked = store.tracking_refs(source)?;
    if tracked.is_empty() {
        anyhow::bail!(
            "nothing is fetched from {name}; fetch first: gage fetch {name}",
            name = repository.name()
        );
    }
    Ok(tracked)
}

/// Preview what the merge takes and confirm. `only` receives the
/// settled selection: the real names of the refs to merge, or `None`
/// for every fetched ref.
fn merge_dialog(
    store: &Store,
    source: &FetchSource,
    tracked: &BTreeMap<String, String>,
    objects: &Objects,
    yes: bool,
    only: &mut Option<Option<HashSet<String>>>,
) -> Result<DialogResult, DialogError> {
    let remote = source.repository().name();
    let selected = match objects {
        Objects::All => {
            cli::log::remark(format!(
                "{} fetched from {remote}",
                ref_counts(tracked.keys())
            ))?;
            None
        }
        Objects::Named(objects) => Some(
            named_closure(
                store,
                source,
                tracked,
                objects,
                &format!("in the fetch of {remote}"),
            )
            .map_err(DialogError::Failed)?,
        ),
    };
    if let Some(selected) = &selected {
        let mut names: Vec<&String> = selected.iter().collect();
        names.sort();
        dialog::step_list("Objects", names.into_iter().map(|r| ref_label(r)))?;
    }

    dialog::confirm(yes, true)?;
    *only = Some(selected);
    Ok(format!("Merging from {remote}").into())
}

/// The fetched refs `objects` name and everything they link among the
/// fetched refs. An error is the lines to show.
fn named_closure(
    store: &Store,
    source: &FetchSource,
    tracked: &BTreeMap<String, String>,
    objects: &[String],
    place: &str,
) -> Result<HashSet<String>, String> {
    let named = resolve_objects(objects, tracked, place)?;
    link_closure(store, source, &named, tracked, |_| Ok(())).map_err(|e| e.to_string())
}

/// Decide the local refs from the tracking refs of `source`: every
/// one, or those named in `only`. `choose` resolves a divergence that
/// would otherwise be a conflict. An error is the lines to show.
pub fn merge(
    store: &Store,
    source: &FetchSource,
    only: Option<&HashSet<String>>,
    choose: Option<Side>,
) -> Result<MergeReport, String> {
    let author = resolve_author(None);
    store
        .merge_fetched(source, only, &author, choose)
        .map_err(|e| e.to_string())
}

/// The command a report speaks for
#[derive(Clone, Copy)]
pub enum Run {
    Merge,
    Pull,
}

impl Run {
    fn command(self) -> &'static str {
        match self {
            Run::Merge => "merge",
            Run::Pull => "pull",
        }
    }

    fn done(self) -> &'static str {
        match self {
            Run::Merge => "Merged",
            Run::Pull => "Pulled",
        }
    }
}

/// One line per conflict, then a count per outcome.
pub fn print_report(run: Run, source: &str, report: &MergeReport) {
    for line in report_lines(run, source, report) {
        println!("{line}");
    }
}

fn report_lines(run: Run, source: &str, report: &MergeReport) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for outcome in &report.outcomes {
        let label = match &outcome.outcome {
            Outcome::Created => "created",
            Outcome::UpToDate => "up to date",
            Outcome::Ahead => "ahead",
            Outcome::FastForwarded => "fast-forwarded",
            Outcome::Tombstoned => "deleted",
            Outcome::Resurrected => "restored",
            Outcome::Replaced => "replaced",
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
                lines.push(format!("conflict: {subject}: {what}"));
                "conflicts"
            }
        };
        *counts.entry(label).or_insert(0) += 1;
    }
    if counts.is_empty() {
        lines.push(format!("Nothing to {} from {source}", run.command()));
        return lines;
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(label, n)| format!("{n} {label}"))
        .collect();
    lines.push(format!(
        "{} from {source}: {}",
        run.done(),
        parts.join(", ")
    ));
    if report.conflicts().next().is_some() {
        lines.push(format!(
            "Resolve a conflict with: gage {} {source} <OBJECT>... --ours | --theirs",
            run.command()
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::fs;

    use gage_store::{NoteEdit, NoteStore, NoteValue, OBJECT_REFS, TagStore};

    use super::*;
    use crate::cmd_fetch::tests::{note, object_ref, open_store, pair, plan};
    use crate::cmd_fetch::transfer;

    fn edit_note(store: &Store, id: &str, value: &str) {
        NoteStore::from(store)
            .edit(
                id,
                NoteEdit {
                    value: Some(NoteValue::Text(value.into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap();
    }

    fn up() -> FetchSource {
        FetchSource::from(Repository::from_remote("up"))
    }

    fn fetch_all(store: &Store) {
        let source = up();
        transfer(store, &source, &plan(store, &source, &Objects::All)).unwrap();
    }

    /// A merge takes what the fetch tracked with the remote's
    /// repository gone, so nothing of it is read over a transport.
    #[test]
    fn a_merge_takes_the_fetched_refs_without_the_remote() {
        let (tmp, up_store, down) = pair();
        let id = note(&up_store, "n");
        let remote_tip = up_store.rev_parse(&object_ref(&id)).unwrap();
        fetch_all(&down);
        drop(up_store);
        fs::remove_dir_all(tmp.path().join("up.git")).unwrap();

        let source = up();
        let tracked = fetched_refs(&down, &source).unwrap();
        assert!(tracked.contains_key(&object_ref(&id)));
        let report = merge(&down, &source, None, None).unwrap();

        assert_eq!(down.rev_parse(&object_ref(&id)).unwrap(), remote_tip);
        assert_eq!(down.resolve_id(&id).unwrap().0, id);
        assert_eq!(
            report_lines(Run::Merge, "up", &report),
            ["Merged from up: 1 created"]
        );
        assert_eq!(
            report_lines(
                Run::Merge,
                "up",
                &merge(&down, &source, None, None).unwrap()
            ),
            ["Merged from up: 1 up to date"]
        );
    }

    #[test]
    fn a_merge_of_a_remote_never_fetched_names_gage_fetch() {
        let (_tmp, up_store, down) = pair();
        note(&up_store, "n");

        let err = fetched_refs(&down, &up()).unwrap_err().to_string();

        assert_eq!(
            err,
            "nothing is fetched from up; fetch first: gage fetch up"
        );
    }

    #[test]
    fn a_merge_of_a_url_is_refused_and_names_pull() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path(), "store");
        let source = FetchSource::from(Repository::from_url("/srv/url.git").unwrap());

        let err = fetched_refs(&store, &source).unwrap_err().to_string();

        assert!(err.contains("/srv/url.git is not a remote"), "{err}");
        assert!(err.contains("gage pull"), "{err}");
    }

    /// Named objects resolve against the fetched refs, and the merge
    /// takes them with what they link and leaves the rest of the
    /// fetch unmerged.
    #[test]
    fn a_named_merge_takes_the_object_and_what_it_links() {
        let (_tmp, up_store, down) = pair();
        let id = note(&up_store, "n");
        let other = note(&up_store, "other");
        TagStore::from(&up_store).add("v1", &id, false).unwrap();
        fetch_all(&down);
        let source = up();
        let tracked = fetched_refs(&down, &source).unwrap();

        let only = named_closure(&down, &source, &tracked, &["v1".to_string()], "here").unwrap();
        let expected: HashSet<String> = [format!("{TAG_REFS}v1"), object_ref(&id)].into();
        assert_eq!(only, expected);
        merge(&down, &source, Some(&only), None).unwrap();

        assert!(down.rev_parse(&object_ref(&id)).unwrap().is_some());
        assert_eq!(down.rev_parse(&object_ref(&other)).unwrap(), None);
        assert_eq!(TagStore::from(&down).resolve("v1").unwrap().unwrap().id, id);

        let err = named_closure(
            &down,
            &source,
            &tracked,
            &["zzzz".to_string()],
            "in the fetch of up",
        )
        .unwrap_err();
        assert_eq!(err, "zzzz: not found in the fetch of up");
    }

    /// A divergence a note cannot merge is a conflict that names the
    /// merge to resolve it, and a side given to that merge settles
    /// it from the same fetched refs.
    #[test]
    fn a_conflict_names_the_merge_that_resolves_it() {
        let (_tmp, up_store, down) = pair();
        let id = note(&up_store, "n");
        fetch_all(&down);
        let source = up();
        merge(&down, &source, None, None).unwrap();
        edit_note(&up_store, &id, "theirs");
        edit_note(&down, &id, "ours");
        fetch_all(&down);

        let report = merge(&down, &source, None, None).unwrap();
        let lines = report_lines(Run::Merge, "up", &report);
        assert_eq!(
            lines.last().unwrap(),
            "Resolve a conflict with: gage merge up <OBJECT>... --ours | --theirs"
        );
        assert!(lines[0].starts_with("conflict: note "), "{lines:?}");

        let only: HashSet<String> = [format!("{OBJECT_REFS}{id}")].into();
        let report = merge(&down, &source, Some(&only), Some(Side::Theirs)).unwrap();
        assert_eq!(
            report_lines(Run::Merge, "up", &report),
            ["Merged from up: 1 resolved"]
        );
        assert_eq!(
            down.rev_parse(&object_ref(&id)).unwrap(),
            up_store.rev_parse(&object_ref(&id)).unwrap()
        );
    }

    #[test]
    fn an_empty_report_says_nothing_to_do_in_the_words_of_the_run() {
        let report = MergeReport::default();
        assert_eq!(
            report_lines(Run::Merge, "up", &report),
            ["Nothing to merge from up"]
        );
        assert_eq!(
            report_lines(Run::Pull, "/srv/up.git", &report),
            ["Nothing to pull from /srv/up.git"]
        );
    }
}
