use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{ArgGroup, Args, Subcommand};
use cliclack as cli;
use datafusion::arrow::array::{
    Array, BooleanArray, Int64Array, StringArray, TimestampMillisecondArray,
};
use gage_claude::driver::ClaudeDriver;
use gage_core::uuid::short_uuid;
use gage_query2::ContextBuilder;
use gage_registry::scanner::{
    Scanner, ScannerDef, ScannerRegistry, parse_scanner_file, split_scanner_spec,
};
use gage_runtime2::{Output, TaskOutput};
use gage_scan2::scan_dir::scans_dir;
use gage_scan2::{CompiledScanner, Event, ScanConfig, ScanOutput, summary_line};
use gage_store::{DatasetStore, SCAN_TYPE, ScanStore, Store};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
    },
};

use crate::cmd_dataset;
use crate::cmd_note::count_rows;
use crate::cmd_session::{column, run_query};
use crate::dialog::{self, DialogError};
use crate::human::{format_duration, format_elapsed_ms};
use crate::session_select::{SELECT_ARG_NAMES, SessionSelectArgs};
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
    /// Delete scan runs
    ///
    /// Deletes each scan run and the notes and issues it wrote. Notes
    /// carried from earlier scan runs are kept.
    Delete(Scan2DeleteArgs),
}

#[derive(Args)]
#[command(group = ArgGroup::new("scan2_dataset")
    .required(true)
    .multiple(true)
    .args(SELECT_ARG_NAMES.iter().copied().chain(["dataset", "no_dataset", "list_scanners"])))]
pub struct Scan2RunArgs {
    /// Scanner to run (repeatable)
    #[arg(short, long = "scanner", value_name = "NAME", display_order = 2)]
    scanners: Vec<String>,

    /// Dataset to scan (ID or prefix)
    ///
    /// Runs the scan against an existing dataset instead of creating
    /// one from the session-selection options.
    #[arg(
        short,
        long,
        value_name = "DATASET",
        display_order = 3,
        conflicts_with_all = SELECT_ARG_NAMES,
    )]
    dataset: Option<String>,

    /// Scanner file to run (repeatable)
    #[arg(short, long = "file", value_name = "PATH", display_order = 4)]
    files: Vec<String>,

    /// Tasks to run at once
    #[arg(short, long, value_name = "N", default_value_t = 10, display_order = 5)]
    jobs: usize,

    #[command(flatten)]
    select: SessionSelectArgs,

    /// Run the scan without a dataset
    ///
    /// No dataset is created or selected; scanners run with no
    /// sessions to read.
    #[arg(
        long,
        display_order = 12,
        conflicts_with = "dataset",
        conflicts_with_all = SELECT_ARG_NAMES,
    )]
    no_dataset: bool,

    /// Run only the scanners named, without pulling in required_by dependents
    #[arg(long, display_order = 13)]
    no_deps: bool,

    /// Ignore prior work
    ///
    /// Every session is scanned in full regardless of earlier scans,
    /// and no notes from earlier scans are carried forward.
    #[arg(long, display_order = 14)]
    invalidate: bool,

    /// Show available scanners and exit
    #[arg(long, exclusive = true, display_order = 15)]
    list_scanners: bool,
}

#[derive(Args)]
pub struct Scan2ListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,
}

#[derive(Args)]
pub struct Scan2DeleteArgs {
    /// Scan run IDs (or prefixes)
    #[arg(required = true)]
    ids: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub async fn main(args: Scan2Args) {
    match args.command {
        Some(Scan2Command::List(a)) => list(a).await,
        Some(Scan2Command::Delete(a)) => delete(a),
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
    // Sessions and Notes are counts over the relation views
    let sql = format!(
        "SELECT s.id, s.id_prefix, s.tasks, s.failed, s.canceled, s.started, s.stopped, \
                ss.n, sn.n \
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
        let sessions = column::<Int64Array>(batch, 7);
        let notes = column::<Int64Array>(batch, 8);
        for i in 0..batch.num_rows() {
            let count = |arr: &Int64Array| if arr.is_valid(i) { arr.value(i) } else { 0 };
            let elapsed = stopped.value(i).saturating_sub(started.value(i)).max(0) as u64;
            rows.push(vec![
                s::styled_id(short_uuid(ids.value(i)), prefixes.value(i), s::IdKind::Gage),
                tasks.value(i).to_string(),
                count(sessions).to_string(),
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

fn delete(args: Scan2DeleteArgs) {
    let store = open_store("gage scan2 delete");
    let scans = ScanStore::from(&store);

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut ids: Vec<String> = Vec::with_capacity(args.ids.len());
    let mut errors = 0;
    for prefix in &args.ids {
        match scans.get(prefix) {
            Ok(record) => ids.push(record.id),
            Err(e) => {
                eprintln!("gage scan2 delete: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let count = ids.len();
    dialog::run("Delete scan runs", || {
        cli::log::remark(format!("{count} {}", plural(count, "scan run")))?;

        if !args.yes {
            let prompt = format!(
                "Permanently delete {count} {} and associated notes/issues? \
                 This cannot be undone.",
                plural(count, "scan run")
            );
            let confirmed = cli::confirm(prompt).initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let mut deleted = 0;
        let mut notes = 0;
        let mut issues = 0;
        for id in &ids {
            match scans.delete_cascade(id) {
                Ok(d) => {
                    deleted += 1;
                    notes += d.notes.len();
                    issues += d.issues.len();
                }
                Err(e) => eprintln!("warning: failed to delete {}: {e}", short_uuid(id)),
            }
        }

        Ok(format!(
            "Deleted {deleted} {}, {notes} {}, {issues} {}",
            plural(deleted, "scan run"),
            plural(notes, "note"),
            plural(issues, "issue")
        )
        .into())
    });
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        noun.to_string()
    } else {
        format!("{noun}s")
    }
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

    // Either --dataset names an existing dataset, the
    // session-selection options mint one populated with the
    // matching native sessions, or --no-dataset runs with none.
    // The clap group guarantees one of the three is present.
    let dataset_sha = if args.no_dataset {
        None
    } else if let Some(prefix) = args.dataset.as_deref() {
        let record = match DatasetStore::from(&store).get(prefix) {
            Ok(record) => record,
            Err(e) => {
                eprintln!("gage scan2: --dataset {prefix}: {e}");
                std::process::exit(1);
            }
        };
        let members = match DatasetStore::from(&store).sessions_list(&record.id) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("gage scan2: --dataset {prefix}: {e}");
                std::process::exit(1);
            }
        };
        if members.is_empty() {
            println!(
                "Note: dataset {} contains no sessions; the scan will run against an empty set",
                short_uuid(&record.id)
            );
        }
        Some(record.commit_sha)
    } else {
        let id = cmd_dataset::create_dataset("gage scan2", &store);
        println!("Created dataset {}", short_uuid(&id));
        cmd_dataset::populate_dataset("gage scan2", &store, &id, None, &args.select, None).await;
        match DatasetStore::from(&store).get(&id) {
            Ok(record) => Some(record.commit_sha),
            Err(e) => {
                eprintln!("gage scan2: {e}");
                std::process::exit(1);
            }
        }
    };

    // Named scanners come from the registry; `-f` files are parsed on
    // this invocation. Named scanners run first, then files. Either
    // spec may carry a `#{...}` params override suffix.
    let registry = ScannerRegistry::load();
    let mut errors = 0;
    let mut seen: Vec<&str> = Vec::new();
    let mut scanners: Vec<Scanner<'_>> = Vec::new();
    let mut file_defs: Vec<(ScannerDef, &str)> = Vec::new();
    for spec in &args.scanners {
        let (name, params_override) = split_scanner_spec(spec);
        if seen.contains(&name) {
            eprintln!("gage scan2: Scanner '{name}' specified more than once");
            errors += 1;
            continue;
        }
        seen.push(name);
        // Library scanners are not selectable: same error as an
        // unknown name
        match registry.get_def(name) {
            Some(def) if !def.library => match Scanner::from_spec(def, params_override, spec) {
                Ok(scanner) => scanners.push(scanner),
                Err(e) => {
                    eprintln!("gage scan2: {e}");
                    errors += 1;
                }
            },
            _ => {
                eprintln!("gage scan2: Unknown scanner: {name}");
                errors += 1;
            }
        }
    }
    for spec in &args.files {
        let (path, _) = split_scanner_spec(spec);
        match parse_scanner_file(&PathBuf::from(path)) {
            Ok(def) => file_defs.push((def, spec)),
            Err(e) => {
                eprintln!("gage scan2: {e}");
                errors += 1;
            }
        }
    }
    for (def, spec) in &file_defs {
        let (_, params_override) = split_scanner_spec(spec);
        match Scanner::from_spec(def, params_override, spec) {
            Ok(scanner) => scanners.push(scanner),
            Err(e) => {
                eprintln!("gage scan2: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }
    let defs: Vec<&ScannerDef> = scanners.iter().map(|s| s.def).collect();

    // Pull in tasks that declare themselves `required_by` what the
    // selection writes. `--no-deps` skips the pull-in so only the
    // named scanners run.
    let required = if args.no_deps {
        Vec::new()
    } else {
        let cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(e) => {
                eprintln!("gage scan2: reading cwd: {e}");
                std::process::exit(1);
            }
        };
        let config = match gage_core::config::load_merged(&cwd) {
            Ok((config, _)) => config,
            Err(e) => {
                eprintln!("gage scan2: reading config: {e}");
                std::process::exit(1);
            }
        };
        registry.required_tasks(&defs, &config)
    };

    // Every scanner compiles before any task runs, so a broken
    // scanner is a full stop.
    let mut compiled: Vec<CompiledScanner> = Vec::new();
    let results = scanners.iter().map(gage_scan2::compile).chain(
        required
            .iter()
            .map(|(def, tasks)| gage_scan2::compile(&Scanner::with_tasks(def, tasks.clone()))),
    );
    for result in results {
        match result {
            Ok(s) => compiled.push(s),
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
        scans_dir: &scans_dir(),
        gage_version: crate::VERSION,
        dataset: dataset_sha.as_deref(),
        jobs: args.jobs,
        driver: Arc::new(ClaudeDriver::new()),
        invalidate: args.invalidate,
    };
    // Headless: task output and the scan's own lines go to the
    // terminal as they happen, unprefixed; records go to the scan
    // record only
    let result = gage_scan2::scan(&store, &config, &compiled, &cancel, |event| match event {
        Event::Output(TaskOutput { output, .. }) => match output {
            Output::Print(s) => print!("{s}"),
            Output::Println(s) => println!("{s}"),
            Output::Log { .. } | Output::Progress { .. } => {}
        },
        Event::Scan(ScanOutput::Out(s)) => print!("{s}"),
        Event::Scan(ScanOutput::Err(s)) => eprint!("{s}"),
        Event::Summary { id, attrs } => {
            println!("{}", summary_line(&styled_scan_id(&store, &id), &attrs))
        }
        Event::Warning {
            scanner,
            task,
            message,
        } => eprintln!("warning: task {scanner}:{task} {message}"),
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

/// The scan's short id, highlighted against the same set a scan id
/// argument resolves against. When that set cannot be read the id is
/// shown plain and the failure goes to stderr.
fn styled_scan_id(store: &Store, id: &str) -> String {
    match store.short_prefix_ids(Some(SCAN_TYPE)) {
        Ok(peers) => s::IdHighlighter::new(peers).short(id),
        Err(e) => {
            eprintln!("warning: reading scan ids: {e}");
            short_uuid(id).to_string()
        }
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
