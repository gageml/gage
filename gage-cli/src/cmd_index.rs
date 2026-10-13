use clap::Args;
use cliclack as cli;
use gage_session::MaintenanceStatus;
use tabled::{
    Table,
    settings::{Style, object::Columns},
};

use crate::human::format_size;
use crate::source;
use crate::style;

#[derive(Args)]
pub struct IndexArgs {
    /// Session source whose index to maintain
    #[arg(short, long, value_name = "SOURCE", long_help = format!("Session source whose index to maintain\n\n{}", source::SOURCE_HELP))]
    source: Option<String>,

    /// Rebuild index
    #[arg(long)]
    rebuild: bool,

    /// Show index status
    #[arg(long)]
    status: bool,
}

/// Run the source's reconcile pass and exit. For post-install setup,
/// cron, and bulk imports — keeps the first-build cost out of
/// interactive queries.
pub async fn run(args: IndexArgs) {
    let source = source::open_source_or_exit("gage index", args.source.as_deref().unwrap_or(""));

    if args.status {
        let status = tokio::task::spawn_blocking(move || source.maintenance_status())
            .await
            .unwrap();
        match status {
            Ok(status) => print_status(&status),
            Err(e) => {
                eprintln!("gage index: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let rebuild = args.rebuild;
    let bar = cli::progress_bar(0);
    bar.start("Building index");
    let bar_for_task = bar.clone();
    let result = tokio::task::spawn_blocking(move || {
        source.reconcile(rebuild, &mut |done, total| {
            if done == 0 {
                bar_for_task.set_length(total as u64);
            }
            bar_for_task.set_position(done as u64);
        })
    })
    .await
    .unwrap();
    bar.clear();

    match result {
        Ok(outcome) => {
            println!(
                "{} sessions ({}): {} indexed, {} removed",
                outcome.discovered,
                format_elapsed(outcome.elapsed_ms),
                outcome.prepared,
                outcome.removed,
            );
        }
        Err(e) => {
            eprintln!("gage index: {e}");
            std::process::exit(1);
        }
    }
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

fn print_status(s: &MaintenanceStatus) {
    let last = match s
        .last_reconcile_ms
        .and_then(chrono::DateTime::from_timestamp_millis)
    {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => "never".to_string(),
    };
    let dir = match &s.state_dir {
        Some(dir) => format!("{} ({})", dir.display(), format_size(s.state_bytes as i64)),
        None => "none".to_string(),
    };
    let rows = [
        [
            "sessions".to_string(),
            format!(
                "{} discovered, {} indexed, {} stale",
                s.discovered, s.prepared, s.stale
            ),
        ],
        ["last reconcile".to_string(), last],
        ["cache dir".to_string(), dir],
    ];
    let table = Table::from_iter(rows)
        .with(Style::rounded().horizontals([]))
        .modify(Columns::first(), style::dim())
        .to_string();
    println!("{table}");
}
