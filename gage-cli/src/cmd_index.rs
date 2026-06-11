use clap::Args;
use gage_index::LockMode;

#[derive(Args)]
pub struct IndexArgs {
    /// Delete the derived store and text index, then rebuild from
    /// scratch
    #[arg(long)]
    rebuild: bool,

    /// Report artifact status without reconciling
    #[arg(long)]
    status: bool,
}

/// Run the reconcile pass (columnar store and text index) and exit.
/// For post-install setup, cron, and bulk imports — keeps the
/// first-build cost out of interactive queries. Progress is reported
/// via `tracing` (set GAGE_LOG=info to see per-session output).
pub async fn run(args: IndexArgs) {
    let store = gage_query::default_index_store();

    if args.status {
        let status = tokio::task::spawn_blocking(move || store.status())
            .await
            .expect("status task");
        println!("{status}");
        return;
    }

    let rebuild = args.rebuild;
    let result = tokio::task::spawn_blocking(move || {
        if rebuild {
            store.rebuild()
        } else {
            store.reconcile(LockMode::Wait)
        }
    })
    .await
    .expect("reconcile task");

    match result {
        Ok(outcome) => {
            println!(
                "{} sessions: {} derived, {} reindexed, {} removed",
                outcome.discovered, outcome.derived, outcome.reindexed, outcome.removed
            );
        }
        Err(e) => {
            eprintln!("gage index: {e}");
            std::process::exit(1);
        }
    }
}
