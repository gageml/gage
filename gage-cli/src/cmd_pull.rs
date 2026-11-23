//! `gage pull`: fetch from a repository and merge what was fetched.
//!
//! The repository is a configured remote or a bare Git URL or path.
//! The objects are selected as `gage fetch` selects them, fetched
//! into the repository's tracking refs, and merged as `gage merge`
//! merges them, in one run. A type is not known before an object is
//! transferred, so a pull by type fetches the whole repository and
//! merges the objects of that type and what they link.
//!
//! A URL is fetched into a transient namespace that is deleted before
//! the command exits, so the store keeps no record of the URL. The
//! namespace is deleted on the error path too; a run that ends another
//! way leaves one behind, and the next URL pull clears it.

use anyhow::Context;
use clap::Args;
use gage_store::{FetchSource, MergeReport, RemoteStore, Side, Store};

use crate::cmd_fetch::{
    Objects, TransferPlan, plan_transfer, prefix_lines, selected_objects, transfer,
};
use crate::cmd_merge::{ResolveArgs, Run, TypeArgs, merge, print_report, typed_closure};
use crate::dialog;
use crate::error::CliError;

#[derive(Args)]
pub struct PullArgs {
    /// Remote or URL to pull from
    ///
    /// A remote name from `gage remote add`, or any Git URL or path.
    /// A URL is pulled without adding a remote, and nothing of it
    /// stays in the store but the merged objects
    repository: String,

    /// Objects to pull (tag, ID, or prefix)
    ///
    /// Tag names, object IDs, or unique ID prefixes as the remote
    /// has them. Everything an object links is pulled with it. With
    /// none, objects that remote records as its fetch default are
    /// pulled; pass --all to pull the whole remote
    objects: Vec<String>,

    /// Pull every object and tag from remote
    ///
    /// Required when no object is named and remote records no fetch
    /// default. The pull still previews the
    /// counts and asks to continue
    #[arg(short, long, conflicts_with = "objects")]
    all: bool,

    #[command(flatten)]
    types: TypeArgs,

    #[command(flatten)]
    resolve: ResolveArgs,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub fn main(args: PullArgs) -> Result<(), CliError> {
    let store = Store::open(&gage_store::store_path()).context("gage pull")?;
    let remotes = RemoteStore::from(&store);
    let source = FetchSource::from(remotes.resolve(&args.repository).context("gage pull")?);
    let objects = match args.types.objects() {
        Some(typed) => typed,
        None => selected_objects(
            &remotes,
            source.repository(),
            args.objects,
            args.all,
            "pull",
        )
        .context("gage pull")?,
    };

    if source.is_transient() {
        remotes.clear_transient().context("gage pull")?;
    }
    let remote = source.repository().name().to_string();
    // The dialog previews and confirms; the fetch runs after it, so
    // git's own output lands outside the dialog
    let mut plan: Option<TransferPlan> = None;
    dialog::run(&format!("Pull from {remote}"), || {
        plan_transfer(&remotes, &source, &objects, args.yes, &mut plan)
    })?;
    let Some(plan) = plan else {
        return Ok(());
    };

    let result = pull(&store, &source, &objects, &plan, args.resolve.side());
    let discarded = remotes.discard(&source);
    let report = match result {
        Ok(report) => report,
        Err(message) => {
            let mut lines = prefix_lines("gage pull", &message);
            if let Err(e) = discarded {
                lines.push_str(&format!("\ngage pull: {e}"));
            }
            return Err(CliError::message(lines));
        }
    };
    print_report(Run::Pull, &remote, &report);
    discarded.context("gage pull")?;
    if report.conflicts().next().is_some() {
        return Err(CliError::exit(1));
    }
    Ok(())
}

/// Fetch what `plan` settled and merge exactly what the fetch took,
/// or of a fetch by type the objects of the types and what they link.
/// An error is the lines to show.
fn pull(
    store: &Store,
    source: &FetchSource,
    objects: &Objects,
    plan: &TransferPlan,
    choose: Option<Side>,
) -> Result<MergeReport, String> {
    let fetched = transfer(store, source, plan)?;
    let only = match objects {
        Objects::Typed(types) => {
            let tracked = store.tracking_refs(source).map_err(|e| e.to_string())?;
            Some(typed_closure(
                store,
                source,
                &tracked,
                types,
                "on the remote",
            )?)
        }
        Objects::All | Objects::Named(_) => fetched,
    };
    merge(store, source, only.as_ref(), choose)
}

#[cfg(test)]
mod tests {
    use gage_store::{Outcome, Repository, TagStore};

    use super::*;
    use crate::cmd_fetch::tests::{issue, note, object_ref, pair, plan};

    fn pull_objects(store: &Store, source: &FetchSource, objects: &Objects) -> MergeReport {
        pull(store, source, objects, &plan(store, source, objects), None).unwrap()
    }

    /// A pull of a remote fetches and merges in one run, as a fetch
    /// followed by a merge would.
    #[test]
    fn a_pull_fetches_and_merges() {
        let (_tmp, up, down) = pair();
        let id = note(&up, "n");
        let source = FetchSource::from(Repository::from_remote("up"));

        let report = pull_objects(&down, &source, &Objects::All);

        assert_eq!(
            down.rev_parse(&object_ref(&id)).unwrap(),
            up.rev_parse(&object_ref(&id)).unwrap()
        );
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(report.outcomes[0].outcome, Outcome::Created);
        assert!(
            down.tracking_refs(&source)
                .unwrap()
                .contains_key(&object_ref(&id))
        );
    }

    /// A named pull merges the named object and what it links and
    /// nothing else the remote has.
    #[test]
    fn a_named_pull_merges_only_what_the_names_reach() {
        let (_tmp, up, down) = pair();
        let id = note(&up, "n");
        let other = note(&up, "other");
        TagStore::from(&up).add("v1", &id, false).unwrap();
        let source = FetchSource::from(Repository::from_remote("up"));

        pull_objects(&down, &source, &Objects::Named(vec!["v1".to_string()]));

        assert!(down.rev_parse(&object_ref(&id)).unwrap().is_some());
        assert_eq!(down.rev_parse(&object_ref(&other)).unwrap(), None);
        assert_eq!(TagStore::from(&down).resolve("v1").unwrap().unwrap().id, id);
    }

    /// A pull by type transfers the whole remote, and merges the
    /// objects of the type with what they link and nothing else.
    #[test]
    fn a_typed_pull_merges_only_what_the_objects_of_the_type_reach() {
        let (_tmp, up, down) = pair();
        let cited = note(&up, "cited");
        let other = note(&up, "other");
        let id = issue(&up, "a", std::slice::from_ref(&cited));
        let source = FetchSource::from(Repository::from_remote("up"));

        let report = pull_objects(&down, &source, &Objects::Typed(vec!["issue".to_string()]));

        assert_eq!(report.outcomes.len(), 2);
        assert!(down.rev_parse(&object_ref(&id)).unwrap().is_some());
        assert!(down.rev_parse(&object_ref(&cited)).unwrap().is_some());
        assert_eq!(down.rev_parse(&object_ref(&other)).unwrap(), None);
        assert!(
            down.tracking_refs(&source)
                .unwrap()
                .contains_key(&object_ref(&other))
        );
    }

    /// A pull of a URL merges through a transient namespace, and
    /// discarding it leaves no tracking ref of the URL behind.
    #[test]
    fn a_pull_of_a_url_merges_and_keeps_no_tracking_refs() {
        let (_tmp, up, down) = pair();
        let id = note(&up, "n");
        let remotes = RemoteStore::from(&down);
        let source = FetchSource::from(remotes.resolve(up.path().to_str().unwrap()).unwrap());
        assert!(source.is_transient());

        pull_objects(&down, &source, &Objects::All);
        assert_eq!(remotes.discard(&source).unwrap(), 1);

        assert!(down.rev_parse(&object_ref(&id)).unwrap().is_some());
        assert!(down.tracking_refs(&source).unwrap().is_empty());
    }
}
