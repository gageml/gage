use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand};
use gage_registry::scanner::{ScannerRegistry, parse_scanner_file};
use gage_runtime2::Output;
use gage_scan2::staging::staging_root;
use gage_scan2::{CompiledScanner, Event, ScanOutcome};
use gage_store::{SCAN_TYPE, ScanRecord, ScanStore, Store, TaskStatus};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
    },
};

use crate::human::{format_duration, format_elapsed_ms};
use crate::style as s;

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Scan2Args {
    #[command(subcommand)]
    command: Option<Scan2Command>,

    #[command(flatten)]
    run_args: Scan2RunArgs,
}

#[derive(Subcommand)]
enum Scan2Command {
    /// List scan runs
    List(Scan2ListArgs),
}

#[derive(Args)]
pub struct Scan2RunArgs {
    /// Scanner file to run (repeatable)
    #[arg(short, long = "file", value_name = "PATH")]
    files: Vec<PathBuf>,

    /// Show available scanners and exit
    #[arg(long)]
    list_scanners: bool,
}

#[derive(Args)]
pub struct Scan2ListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,
}

pub async fn main(args: Scan2Args) {
    match args.command {
        Some(Scan2Command::List(a)) => list(a),
        None => run_scan(args.run_args).await,
    }
}

fn list(args: Scan2ListArgs) {
    let store = open_store("gage scan2 list");
    let scans = ScanStore::from(&store);
    let total = match scans.query().count() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("gage scan2 list: {e}");
            std::process::exit(1);
        }
    };
    if total == 0 {
        println!("No scan runs found");
        return;
    }
    let show = args.limit.show_count(total);
    let records: Vec<ScanRecord> =
        match scans.query().limit(show).iter().and_then(|it| it.collect()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("gage scan2 list: {e}");
                std::process::exit(1);
            }
        };

    // The highlighted prefix is unique within the short-prefix set
    // of scans, where a scan prefix resolves first
    let peers = match store.short_prefix_ids(Some(SCAN_TYPE)) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("gage scan2 list: {e}");
            std::process::exit(1);
        }
    };
    let highlighter = s::IdHighlighter::new(peers);

    let header: Vec<String> = [
        "Id", "Tasks", "Sessions", "Issues", "Notes", "Errors", "Cost", "Status", "Duration",
        "Label", "Created",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let rows = records.iter().map(|r| list_row(r, &highlighter));

    let term_width = console::Term::stdout().size().1 as usize;
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .with(
            Width::truncate(term_width)
                .suffix("…")
                .priority(s::IdAwarePriority::new(true)),
        )
        .modify(Rows::first(), s::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(1..7), Alignment::right())
        .modify(Columns::one(7).not(Rows::first()), s::dim())
        .modify(Columns::last().not(Rows::first()), s::dim())
        .to_string();
    println!("{table}");

    args.limit.print_summary(records.len(), total, "scan run");
}

/// One listing row. Sessions, Issues, Notes, Cost, and Label are
/// blank: nothing writes them yet.
fn list_row(record: &ScanRecord, highlighter: &s::IdHighlighter) -> Vec<String> {
    let attrs = &record.content.attrs;
    let status = if attrs.canceled {
        "canceled"
    } else {
        "completed"
    };
    let elapsed = attrs.stopped.saturating_sub(attrs.started).max(0) as u64;
    vec![
        highlighter.short(&record.id),
        attrs.tasks.total.to_string(),
        String::new(),
        String::new(),
        String::new(),
        attrs.tasks.failed.to_string(),
        String::new(),
        status.to_string(),
        format_duration(Duration::from_millis(elapsed)),
        String::new(),
        format_elapsed_ms(attrs.started),
    ]
}

async fn run_scan(args: Scan2RunArgs) {
    if args.list_scanners {
        crate::cmd_scan::list_scanners(&ScannerRegistry::load());
        return;
    }
    if args.files.is_empty() {
        eprintln!("gage scan2: at least one --file is required");
        std::process::exit(2);
    }

    // The store is needed only at the end, but a missing store is a
    // full stop before any work.
    let store = open_store("gage scan2");

    let mut defs = Vec::new();
    let mut errors = 0;
    for path in &args.files {
        match parse_scanner_file(path) {
            Ok(def) => defs.push(def),
            Err(e) => {
                eprintln!("gage scan2: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    // Every scanner compiles before any task runs, so a broken
    // scanner is a full stop.
    let mut scanners: Vec<CompiledScanner> = Vec::new();
    for def in &defs {
        match gage_scan2::compile(def) {
            Ok(s) => scanners.push(s),
            Err(e) => {
                eprintln!("gage scan2: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    // Ctrl-C cancels the run; the scan applies what ran. Once tokio
    // has taken the signal, a second Ctrl-C during apply has no
    // effect.
    let cancel = crate::panic_token().child_token();
    let signal_task = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("gage scan2: canceling");
                    cancel.cancel();
                }
                _ = cancel.cancelled() => {}
            }
        })
    };

    let result = gage_scan2::scan(
        &store,
        &staging_root(),
        &scanners,
        &cancel,
        |event| match event {
            Event::Output(Output::Print(s)) => print!("{s}"),
            Event::Output(Output::Println(s)) => println!("{s}"),
            Event::TaskStarted { .. } => {}
            Event::TaskFinished {
                scanner,
                task,
                status: TaskStatus::Failed,
                error,
            } => {
                let message = error.unwrap_or_default();
                eprintln!("gage scan2: {scanner}:{task}: {message}");
            }
            Event::TaskFinished { .. } => {}
        },
    )
    .await;
    io::stdout().flush().unwrap();
    cancel.cancel();
    signal_task.await.unwrap();

    let outcome = match result {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("gage scan2: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("{}", summary_line(&outcome));
    let attrs = &outcome.attrs;
    if attrs.canceled || attrs.tasks.failed > 0 {
        std::process::exit(1);
    }
}

fn summary_line(outcome: &ScanOutcome) -> String {
    let attrs = &outcome.attrs;
    let state = if attrs.canceled {
        "canceled"
    } else {
        "completed"
    };
    let counts = &attrs.tasks;
    let mut parts = vec![
        format!("{} completed", counts.completed),
        format!("{} failed", counts.failed),
    ];
    let canceled = counts.total - counts.completed - counts.failed - counts.skipped;
    if canceled > 0 {
        parts.push(format!("{canceled} canceled"));
    }
    format!(
        "scan {} {state}: {} tasks: {}",
        outcome.id,
        counts.total,
        parts.join(", ")
    )
}

fn open_store(command: &str) -> Store {
    match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    }
}
