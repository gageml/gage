use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Args, Subcommand};
use datafusion::arrow::array::{
    Array, BooleanArray, Int64Array, StringArray, TimestampMillisecondArray,
};
use gage_core::uuid::short_uuid;
use gage_query2::ContextBuilder;
use gage_registry::scanner::{ScannerDef, ScannerRegistry, parse_scanner_file};
use gage_runtime2::{Output, TaskOutput};
use gage_scan2::staging::staging_root;
use gage_scan2::{CompiledScanner, Event, ScanConfig, ScanOutput};
use gage_store::{DatasetStore, Store};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
    },
};

use crate::cmd_note::count_rows;
use crate::cmd_session::{column, run_query};
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
        Some(Scan2Command::List(a)) => list(a).await,
        None => run_scan(args.run_args).await,
    }
}

async fn list(args: Scan2ListArgs) {
    let store = open_store("gage scan2 list");
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let total = count_rows(&ctx, "SELECT COUNT(*) FROM scan").await;
    if total == 0 {
        println!("No scan runs found");
        return;
    }
    let show = args.limit.show_count(total);
    // Sessions and Notes are counts over the relation views; a scan
    // without a dataset has no session count to show
    let sql = format!(
        "SELECT s.id, s.id_prefix, s.tasks, s.failed, s.canceled, s.started, s.stopped, \
                s.dataset, ss.n, sn.n \
         FROM scan s \
         LEFT JOIN (SELECT scan_id, COUNT(*) AS n FROM scan_session GROUP BY scan_id) ss \
              ON ss.scan_id = s.id \
         LEFT JOIN (SELECT scan_id, COUNT(*) AS n FROM scan_note GROUP BY scan_id) sn \
              ON sn.scan_id = s.id \
         ORDER BY s.modified DESC LIMIT {show}"
    );
    let batches = run_query(&ctx, &sql).await;

    let header: Vec<String> = [
        "Id", "Tasks", "Sessions", "Issues", "Notes", "Errors", "Cost", "Status", "Duration",
        "Label", "Created",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in &batches {
        let ids = column::<StringArray>(batch, 0);
        let prefixes = column::<StringArray>(batch, 1);
        let tasks = column::<Int64Array>(batch, 2);
        let failed = column::<Int64Array>(batch, 3);
        let canceled = column::<BooleanArray>(batch, 4);
        let started = column::<TimestampMillisecondArray>(batch, 5);
        let stopped = column::<TimestampMillisecondArray>(batch, 6);
        let datasets = column::<StringArray>(batch, 7);
        let sessions = column::<Int64Array>(batch, 8);
        let notes = column::<Int64Array>(batch, 9);
        for i in 0..batch.num_rows() {
            let count = |arr: &Int64Array| if arr.is_valid(i) { arr.value(i) } else { 0 };
            let elapsed = stopped.value(i).saturating_sub(started.value(i)).max(0) as u64;
            rows.push(vec![
                s::styled_id(short_uuid(ids.value(i)), prefixes.value(i), s::IdKind::Gage),
                tasks.value(i).to_string(),
                if datasets.is_valid(i) {
                    count(sessions).to_string()
                } else {
                    String::new()
                },
                String::new(),
                count(notes).to_string(),
                failed.value(i).to_string(),
                String::new(),
                if canceled.value(i) {
                    "canceled"
                } else {
                    "completed"
                }
                .to_string(),
                format_duration(Duration::from_millis(elapsed)),
                String::new(),
                format_elapsed_ms(started.value(i)),
            ]);
        }
    }
    let shown = rows.len();

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

    args.limit.print_summary(shown, total, "scan run");
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
