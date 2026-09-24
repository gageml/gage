use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand};
use gage_registry::scanner::{ScannerDef, ScannerRegistry, parse_scanner_file};
use gage_runtime2::{Output, TaskOutput};
use gage_scan2::staging::staging_root;
use gage_scan2::{CompiledScanner, Event, ScanConfig, ScanOutput};
use gage_store::{DatasetStore, SCAN_TYPE, ScanRecord, ScanStore, Store};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
    },
};

use crate::human::{format_duration, format_elapsed_ms};
use crate::style as s;

/// Install the `tracing` subscriber for a scan: warnings and above to
/// stderr, and the records layer into the running scan's staging at
/// `info` and above for the Gage crates. `GAGE_LOG` (set by `--log`)
/// overrides both.
pub fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Layer, fmt};

    let stderr = fmt::layer()
        .with_writer(std::io::stderr)
        .without_time()
        .with_filter(
            EnvFilter::try_from_env("GAGE_LOG").unwrap_or_else(|_| EnvFilter::new("warn")),
        );
    let records =
        gage_scan2::trace::layer().with_filter(EnvFilter::try_from_env("GAGE_LOG").unwrap_or_else(
            |_| EnvFilter::new("warn,gage_store=info,gage_scan2=info,gage_runtime2=info"),
        ));
    tracing_subscriber::registry()
        .with(stderr)
        .with(records)
        .init();
}

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
    /// Scanner to run (repeatable)
    #[arg(short, long = "scanner", value_name = "NAME")]
    scanners: Vec<String>,

    /// Scanner file to run (repeatable)
    #[arg(short, long = "file", value_name = "PATH")]
    files: Vec<PathBuf>,

    /// Dataset to scan (ID or prefix)
    #[arg(short, long, value_name = "DATASET")]
    dataset: Option<String>,

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

    // Sessions is the member count of the dataset commit the scan links
    let datasets = DatasetStore::from(&store);
    let mut session_counts: Vec<Option<usize>> = Vec::with_capacity(records.len());
    for record in &records {
        let count = match &record.content.dataset {
            Some(sha) => match datasets.at_commit(sha) {
                Ok(dataset) => Some(dataset.session_count),
                Err(e) => {
                    eprintln!("gage scan2 list: {e}");
                    std::process::exit(1);
                }
            },
            None => None,
        };
        session_counts.push(count);
    }

    let header: Vec<String> = [
        "Id", "Tasks", "Sessions", "Issues", "Notes", "Errors", "Cost", "Status", "Duration",
        "Label", "Created",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let rows = records
        .iter()
        .zip(&session_counts)
        .map(|(r, sessions)| list_row(r, *sessions, &highlighter));

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

/// One listing row. Sessions is blank for a scan with no dataset;
/// Issues, Cost, and Label are blank: nothing writes them yet.
fn list_row(
    record: &ScanRecord,
    sessions: Option<usize>,
    highlighter: &s::IdHighlighter,
) -> Vec<String> {
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
        sessions.map(|n| n.to_string()).unwrap_or_default(),
        String::new(),
        record.content.notes.len().to_string(),
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
    if args.scanners.is_empty() && args.files.is_empty() {
        eprintln!("gage scan2: at least one --scanner or --file is required");
        std::process::exit(2);
    }

    // The store is needed only at the end, but a missing store is a
    // full stop before any work.
    let store = open_store("gage scan2");

    // The dataset commit the scan links
    let dataset_sha =
        args.dataset
            .as_deref()
            .map(|prefix| match DatasetStore::from(&store).get(prefix) {
                Ok(record) => record.commit_sha,
                Err(e) => {
                    eprintln!("gage scan2: --dataset {prefix}: {e}");
                    std::process::exit(1);
                }
            });

    // Named scanners come from the registry; `-f` files are parsed on
    // this invocation. Named scanners run first, then files.
    let registry = ScannerRegistry::load();
    let mut errors = 0;
    let mut seen: Vec<&str> = Vec::new();
    let mut defs: Vec<&ScannerDef> = Vec::new();
    for name in &args.scanners {
        if name.contains("#{") {
            eprintln!("gage scan2: scanner params are not supported: {name}");
            errors += 1;
            continue;
        }
        if seen.contains(&name.as_str()) {
            eprintln!("gage scan2: Scanner '{name}' specified more than once");
            errors += 1;
            continue;
        }
        seen.push(name);
        // Library scanners are not selectable: same error as an
        // unknown name
        match registry.get_def(name) {
            Some(def) if !def.library => defs.push(def),
            _ => {
                eprintln!("gage scan2: Unknown scanner: {name}");
                errors += 1;
            }
        }
    }
    let mut file_defs = Vec::new();
    for path in &args.files {
        match parse_scanner_file(path) {
            Ok(def) => file_defs.push(def),
            Err(e) => {
                eprintln!("gage scan2: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }
    defs.extend(file_defs.iter());

    // Every scanner compiles before any task runs, so a broken
    // scanner is a full stop.
    let mut scanners: Vec<CompiledScanner> = Vec::new();
    for def in defs {
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
                _ = tokio::signal::ctrl_c() => cancel.cancel(),
                _ = cancel.cancelled() => {}
            }
        })
    };

    let config = ScanConfig {
        staging_root: &staging_root(),
        gage_version: crate::VERSION,
        dataset: dataset_sha.as_deref(),
    };
    // Headless: task output and the scan's own lines go to the
    // terminal as they happen, unprefixed; records go to the scan
    // record only
    let result = gage_scan2::scan(&store, &config, &scanners, &cancel, |event| match event {
        Event::Output(TaskOutput { output, .. }) => match output {
            Output::Print(s) => print!("{s}"),
            Output::Println(s) => println!("{s}"),
            Output::Log { .. } => {}
        },
        Event::Scan(ScanOutput::Out(s)) => print!("{s}"),
        Event::Scan(ScanOutput::Err(s)) => eprint!("{s}"),
        Event::TaskStarted { .. } | Event::TaskFinished { .. } => {}
    })
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
    let attrs = &outcome.attrs;
    if attrs.canceled || attrs.tasks.failed > 0 {
        std::process::exit(1);
    }
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
