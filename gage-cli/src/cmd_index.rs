use std::time::Duration;

use clap::Args;
use gage_index::{LockMode, ReconcileEvent, Status};
use indicatif::{ProgressBar, ProgressStyle};
use tabled::{
    Table,
    settings::{Style, object::Columns},
};

use crate::style;

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
            .unwrap();
        print_status(&status);
        return;
    }

    let rebuild = args.rebuild;
    let bar = make_progress_bar();
    let bar_for_task = bar.clone();
    let result = tokio::task::spawn_blocking(move || {
        let on_event = |event: ReconcileEvent| match event {
            ReconcileEvent::Start { total } => {
                bar_for_task.set_length(total);
                bar_for_task.set_position(0);
            }
            ReconcileEvent::Advance => {
                bar_for_task.inc(1);
            }
        };
        if rebuild {
            store.rebuild_with_progress(on_event)
        } else {
            store.reconcile_with_progress(LockMode::Wait, on_event)
        }
    })
    .await
    .unwrap();
    bar.finish_and_clear();

    match result {
        Ok(outcome) => {
            println!(
                "{} sessions ({}): {} indexed, {} removed",
                outcome.discovered,
                format_elapsed(outcome.elapsed_ms),
                outcome.indexed,
                outcome.removed,
            );
        }
        Err(e) => {
            eprintln!("gage index: {e}");
            std::process::exit(1);
        }
    }
}

fn make_progress_bar() -> ProgressBar {
    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.magenta} {msg} [{elapsed_precise}] {bar:30.white/bright.black} ({pos}/{len})",
        )
        .unwrap()
        .progress_chars("▬▬"),
    );
    bar.set_message("Indexing");
    bar.enable_steady_tick(Duration::from_millis(120));
    bar
}

fn format_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

fn print_status(s: &Status) {
    let rows = [
        [
            "index".to_string(),
            format!(
                "v{} ({} sessions indexed, {}, tokenizer {})",
                s.index_version,
                s.indexed,
                s.index_bytes_display(),
                s.tokenizer_chain,
            ),
        ],
        [
            "sessions".to_string(),
            format!("{} discovered, {} dirty", s.discovered, s.dirty),
        ],
        ["last reconcile".to_string(), s.last_reconcile_display()],
        [
            "cache dir".to_string(),
            format!("{} ({})", s.cache_dir.display(), s.cache_bytes_display()),
        ],
    ];
    let table = Table::from_iter(rows)
        .with(Style::rounded().horizontals([]))
        .modify(Columns::first(), style::dim())
        .to_string();
    println!("{table}");
}
