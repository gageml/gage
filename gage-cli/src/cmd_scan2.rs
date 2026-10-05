use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Args, Subcommand};
use cliclack as cli;
use console::style;
use datafusion::arrow::array::{
    Array, BooleanArray, Int64Array, StringArray, TimestampMillisecondArray,
};
use gage_claude::driver::ClaudeDriver;
use gage_claude::session::SessionInfo;
use gage_core::config::Config;
use gage_core::uuid::short_uuid;
use gage_query2::ContextBuilder;
use gage_registry::scanner::{
    Scanner, ScannerDef, ScannerRegistry, parse_scanner_file, split_scanner_spec,
};
use gage_runtime2::{LOG_TARGET, Output, TaskOutput};
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
use tracing::field::{Field, Visit};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::format::Writer;

use crate::cmd_dataset;
use crate::cmd_note::count_rows;
use crate::cmd_session::{column, run_query};
use crate::dialog::{self, DialogError};
use crate::human::{format_duration, format_elapsed_ms};
use crate::session_select::{SELECT_ARG_NAMES, SessionSelectArgs};
use crate::style as s;

/// Install the `tracing` subscriber for a scan: warnings and above to
/// stderr, and the records layer into the active scan's directory at
/// `info` and above for the Gage crates. `GAGE_LOG` (set by `--log`)
/// overrides both. A scanner's own `log` records reach stderr through
/// a second layer that renders their target as
/// `scanner::<scanner>::<task>`, the scanner code's counterpart of a
/// crate's module path.
pub fn init_logging() {
    use tracing_subscriber::filter::{FilterExt, filter_fn};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Layer, fmt};

    let stderr_filter =
        || EnvFilter::try_from_env("GAGE_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let stderr = fmt::layer()
        .with_writer(io::stderr)
        .without_time()
        .with_filter(stderr_filter().and(filter_fn(|meta| meta.target() != LOG_TARGET)));
    let scanner_stderr = fmt::layer()
        .with_writer(io::stderr)
        .without_time()
        .with_target(false)
        .fmt_fields(ScannerFields)
        .with_filter(stderr_filter().and(filter_fn(|meta| meta.target() == LOG_TARGET)));
    let records =
        gage_scan2::trace::layer().with_filter(EnvFilter::try_from_env("GAGE_LOG").unwrap_or_else(
            |_| EnvFilter::new("warn,gage_store=info,gage_scan2=info,gage_runtime2=info"),
        ));
    tracing_subscriber::registry()
        .with(stderr)
        .with(scanner_stderr)
        .with(records)
        .init();
}

/// Field formatter for a scanner's `log` events: the `scanner` and
/// `task` fields become the target, `scanner::<scanner>::<task>:`,
/// styled as the default formatter styles a target, followed by the
/// message and any other fields as `key=value`.
struct ScannerFields;

impl<'w> FormatFields<'w> for ScannerFields {
    fn format_fields<R: RecordFields>(&self, mut writer: Writer<'w>, fields: R) -> fmt::Result {
        let mut v = ScannerFieldsVisitor::default();
        fields.record(&mut v);
        let target = format!("{LOG_TARGET}::{}::{}", v.scanner, v.task);
        if writer.has_ansi_escapes() {
            write!(writer, "\x1b[2m{target}\x1b[0m\x1b[2m:\x1b[0m")?;
        } else {
            write!(writer, "{target}:")?;
        }
        write!(writer, " {}", v.message)?;
        if !v.rest.is_empty() {
            write!(writer, " {}", v.rest)?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct ScannerFieldsVisitor {
    scanner: String,
    task: String,
    message: String,
    rest: String,
}

impl Visit for ScannerFieldsVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        match field.name() {
            "scanner" => self.scanner = format!("{value:?}"),
            "task" => self.task = format!("{value:?}"),
            "message" => self.message = format!("{value:?}"),
            name => {
                if !self.rest.is_empty() {
                    self.rest.push(' ');
                }
                write!(self.rest, "{name}={value:?}").unwrap();
            }
        }
    }
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
pub struct Scan2RunArgs {
    /// Scanner to run (repeatable)
    #[arg(short, long = "scanner", value_name = "NAME", display_order = 2)]
    scanners: Vec<String>,

    /// Run the scanners in a group (repeatable)
    #[arg(short, long = "group", value_name = "NAME", display_order = 3)]
    groups: Vec<String>,

    /// Dataset to scan (ID or prefix)
    ///
    /// Runs the scan against an existing dataset instead of creating
    /// one from the session-selection options.
    #[arg(
        short,
        long,
        value_name = "DATASET",
        display_order = 4,
        conflicts_with_all = SELECT_ARG_NAMES,
    )]
    dataset: Option<String>,

    /// Scanner file to run (repeatable)
    #[arg(short, long = "file", value_name = "PATH", display_order = 5)]
    files: Vec<String>,

    /// Tasks to run at once
    #[arg(short, long, value_name = "N", default_value_t = 10, display_order = 6)]
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

    /// Skip prompts and confirmation
    ///
    /// Fills unspecified selections with defaults: the `default`
    /// scanner group when none of --scanner, --file, or --group is
    /// set, and the past 30 days capped at 20 sessions when no
    /// session selection or dataset option is set.
    #[arg(short, long, display_order = 15)]
    yes: bool,

    /// Show available scanners and exit
    #[arg(long, exclusive = true, display_order = 16)]
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

    let registry = ScannerRegistry::load();
    let store = open_store("gage scan2");
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

    dialog::run_async("Scan sessions", move || {
        run_scan_dialog(args, registry, config, store)
    })
    .await;
}

/// A chosen session set for the scan: an existing dataset, a new
/// dataset to be built from the resolved native sessions, or none.
/// `Reuse` and `None` carry everything needed to preview the plan;
/// `New` carries the sessions resolved from `SessionSelectArgs`
/// before any dataset is written.
enum DatasetPlan {
    Reuse {
        id: String,
        commit_sha: String,
        session_count: usize,
    },
    New {
        selected: Vec<SessionInfo>,
    },
    None,
}

/// Session-axis window prompt choices.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Window {
    Today,
    Days(u32),
    All,
}

async fn run_scan_dialog(
    mut args: Scan2RunArgs,
    registry: ScannerRegistry,
    config: Config,
    store: Store,
) -> Result<dialog::DialogResult, DialogError> {
    // Scanner selection: -s/-g/-f, the `default` group under -y, or
    // an interactive multi-select when the user supplied nothing.
    let (scanner_specs, prompted) = resolve_scanner_specs(&args, &registry, &config)?;
    let file_defs = parse_file_scanners(&args.files)?;
    let mut scanners: Vec<Scanner<'_>> = Vec::new();
    for spec in &scanner_specs {
        let (name, params_override) = split_scanner_spec(spec);
        let def = registry
            .get_def(name)
            .expect("resolve_scanner_specs validated the name against the registry");
        let scanner = Scanner::from_spec(def, params_override, spec)
            .map_err(|e| DialogError::Failed(format!("{e}")))?;
        scanners.push(scanner);
    }
    for (def, spec) in file_defs.iter().zip(&args.files) {
        let (_, params_override) = split_scanner_spec(spec);
        let scanner = Scanner::from_spec(def, params_override, spec)
            .map_err(|e| DialogError::Failed(format!("{e}")))?;
        scanners.push(scanner);
    }
    // The multi-select has already displayed the user's choice;
    // the preview is for pinned/defaulted selections only.
    if !prompted {
        preview_scanners(&scanners)?;
    }

    // Session / dataset axis: either reuse, mint from a selection,
    // or none. The plan is a preview only; nothing is written until
    // after the confirmation.
    let plan = resolve_dataset_plan(&mut args, &store).await?;
    preview_dataset(&plan)?;

    // Preflight: compile every selected scanner and every pulled-in
    // required_by dependent before any task runs.
    let defs: Vec<&ScannerDef> = scanners.iter().map(|s| s.def).collect();
    let required = if args.no_deps {
        Vec::new()
    } else {
        registry.required_tasks(&defs, &config)
    };
    let mut compiled: Vec<CompiledScanner> = Vec::new();
    let mut errors = 0;
    let compile_results = scanners.iter().map(gage_scan2::compile).chain(
        required
            .iter()
            .map(|(def, tasks)| gage_scan2::compile(&Scanner::with_tasks(def, tasks.clone()))),
    );
    for result in compile_results {
        match result {
            Ok(c) => compiled.push(c),
            Err(e) => {
                cli::log::error(format!("{e}"))?;
                errors += 1;
            }
        }
    }
    if errors > 0 {
        return Err(DialogError::Failed(
            "Scan not started due to scanner errors".to_string(),
        ));
    }

    // Confirmation.
    if !args.yes {
        let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    // Dataset materialization — the one write between the dialog
    // and the scan.
    let dataset_sha = materialize_dataset(plan, &store)?;

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

    let scan_config = ScanConfig {
        scans_dir: &scans_dir(),
        gage_version: crate::VERSION,
        dataset: dataset_sha.as_deref(),
        jobs: args.jobs,
        driver: Arc::new(ClaudeDriver::new()),
        invalidate: args.invalidate,
    };
    // Headless: task output and the scan's own lines go to the
    // terminal as they happen, unprefixed; the dialog outro renders
    // the summary line from the returned outcome.
    let result = gage_scan2::scan(
        &store,
        &scan_config,
        &compiled,
        &cancel,
        |event| match event {
            Event::Output(TaskOutput { output, .. }) => match output {
                Output::Print(s) => print!("{s}"),
                Output::Println(s) => println!("{s}"),
                Output::Log { .. } | Output::Progress { .. } => {}
            },
            Event::Scan(ScanOutput::Out(s)) => print!("{s}"),
            Event::Scan(ScanOutput::Err(s)) => eprint!("{s}"),
            Event::Summary { .. } => {}
            Event::Warning {
                scanner,
                task,
                message,
            } => eprintln!("warning: task {scanner}:{task} {message}"),
            Event::TaskStarted { .. } | Event::TaskFinished { .. } => {}
        },
    )
    .await;
    io::stdout().flush().unwrap();
    cancel.cancel();
    signal_task.await.unwrap();

    let outcome = result.map_err(|e| DialogError::Other(anyhow::anyhow!("{e}")))?;
    let shown_id = styled_scan_id(&store, &outcome.id);
    let summary = summary_line(&shown_id, &outcome.attrs);
    if outcome.attrs.canceled || outcome.attrs.tasks.failed > 0 {
        return Err(DialogError::Failed(summary));
    }
    Ok(summary.into())
}

/// Resolve the ordered list of registry scanner specs from `-s`,
/// `-g`, the `-y` default, or an interactive multi-select when the
/// user supplied no scanner input at all. `-f` paths are handled by
/// the caller and are not part of this list. Returns `(specs,
/// prompted)` where `prompted` is true when the selection came from
/// the multi-select so the caller can skip its own preview.
fn resolve_scanner_specs(
    args: &Scan2RunArgs,
    registry: &ScannerRegistry,
    config: &Config,
) -> Result<(Vec<String>, bool), DialogError> {
    let s_bare: Vec<&str> = args
        .scanners
        .iter()
        .map(|s| split_scanner_spec(s).0)
        .collect();
    for (i, name) in s_bare.iter().enumerate() {
        if s_bare.iter().take(i).any(|n| n == name) {
            return Err(DialogError::Failed(format!(
                "Scanner '{name}' specified more than once"
            )));
        }
    }
    for name in &s_bare {
        match registry.get_def(name) {
            Some(def) if !def.library => (),
            _ => return Err(DialogError::Failed(format!("Unknown scanner: {name}"))),
        }
    }

    let mut group_names: Vec<String> = Vec::new();
    for group in &args.groups {
        let members: Vec<&str> = registry
            .group_members(group)
            .into_iter()
            .filter(|d| config.is_scanner_enabled(&d.name))
            .map(|d| d.name.as_str())
            .collect();
        if members.is_empty() {
            return Err(DialogError::Failed(format!(
                "No scanners for group '{group}'"
            )));
        }
        for name in members {
            if !group_names.iter().any(|n| n == name) {
                group_names.push(name.to_string());
            }
        }
    }

    let no_scanner_input =
        args.scanners.is_empty() && args.groups.is_empty() && args.files.is_empty();
    let mut prompted = false;
    if no_scanner_input {
        if args.yes {
            let members: Vec<&str> = registry
                .group_members("default")
                .into_iter()
                .filter(|d| config.is_scanner_enabled(&d.name))
                .map(|d| d.name.as_str())
                .collect();
            if members.is_empty() {
                return Err(DialogError::Failed(
                    "No scanners in the default group".to_string(),
                ));
            }
            for name in members {
                group_names.push(name.to_string());
            }
        } else {
            let selected = prompt_scanner_multiselect(registry, config)?;
            if selected.is_empty() {
                return Err(DialogError::Failed("No scanners selected".to_string()));
            }
            for name in selected {
                group_names.push(name);
            }
            prompted = true;
        }
    }

    // -s wins over -g on bare-name collision.
    let group_names: Vec<String> = group_names
        .into_iter()
        .filter(|g| !s_bare.contains(&g.as_str()))
        .collect();

    let mut out: Vec<String> = Vec::with_capacity(args.scanners.len() + group_names.len());
    out.extend(args.scanners.iter().cloned());
    out.extend(group_names);
    Ok((out, prompted))
}

/// Multi-select over enabled, non-library scanners with the
/// `default` group pre-selected. The hint shown for each item is
/// the first line of the scanner's description.
fn prompt_scanner_multiselect(
    registry: &ScannerRegistry,
    config: &Config,
) -> Result<Vec<String>, DialogError> {
    let defs = registry.list_enabled(config);
    let mut names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    names.sort();
    let default_indices: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            registry
                .group_members("default")
                .iter()
                .any(|d| d.name == **n)
        })
        .map(|(i, _)| i)
        .collect();
    let mut prompt = cli::multiselect("Scanners").initial_values(default_indices);
    for (i, name) in names.iter().enumerate() {
        let hint = registry
            .get_def(name)
            .map(|d| d.description.lines().next().unwrap_or("").to_string())
            .unwrap_or_default();
        prompt = prompt.item(i, (*name).to_string(), hint);
    }
    let indices: Vec<usize> = prompt.interact()?;
    Ok(indices
        .into_iter()
        .map(|i| {
            names
                .get(i)
                .expect("selected index points into names")
                .to_string()
        })
        .collect())
}

/// Parse `-f` paths into owned `ScannerDef` values. Reports every
/// parse failure in one `Failed` so the user sees them all at once.
fn parse_file_scanners(files: &[String]) -> Result<Vec<ScannerDef>, DialogError> {
    let mut out = Vec::with_capacity(files.len());
    let mut errors: Vec<String> = Vec::new();
    for spec in files {
        let (path, _) = split_scanner_spec(spec);
        match parse_scanner_file(&PathBuf::from(path)) {
            Ok(def) => out.push(def),
            Err(e) => errors.push(format!("{e}")),
        }
    }
    if !errors.is_empty() {
        return Err(DialogError::Failed(errors.join("\n")));
    }
    Ok(out)
}

fn preview_scanners(scanners: &[Scanner<'_>]) -> Result<(), DialogError> {
    let lines: String = scanners
        .iter()
        .map(|s| format!("\n{}", style(&s.def.name).dim()))
        .collect();
    cli::log::step(format!("Scanners{lines}"))?;
    Ok(())
}

/// Decide what the scan's dataset will be. Reuses `--dataset`,
/// honors `--no-dataset`, otherwise prompts the session axis under
/// an empty-and-not-`-y` call and runs `SessionSelectArgs::resolve`
/// for the final native-session list.
async fn resolve_dataset_plan(
    args: &mut Scan2RunArgs,
    store: &Store,
) -> Result<DatasetPlan, DialogError> {
    if args.no_dataset {
        return Ok(DatasetPlan::None);
    }
    if let Some(prefix) = args.dataset.as_deref() {
        let record = DatasetStore::from(store)
            .get(prefix)
            .map_err(|e| DialogError::Failed(format!("--dataset {prefix}: {e}")))?;
        let members = DatasetStore::from(store)
            .sessions_list(&record.id)
            .map_err(|e| DialogError::Failed(format!("--dataset {prefix}: {e}")))?;
        return Ok(DatasetPlan::Reuse {
            id: record.id,
            commit_sha: record.commit_sha,
            session_count: members.len(),
        });
    }

    if args.select.is_empty() && !args.yes {
        prompt_session_axis(&mut args.select)?;
    }
    let selected = args
        .select
        .resolve("gage scan2")
        .await
        .map_err(|e| DialogError::Failed(format!("{e}")))?;
    Ok(DatasetPlan::New { selected })
}

fn prompt_session_axis(select: &mut SessionSelectArgs) -> Result<(), DialogError> {
    let window = cli::select("Timeframe")
        .item(Window::Today, "Today", "")
        .item(Window::Days(7), "This week", "")
        .item(Window::Days(30), "30 days", "")
        .item(Window::All, "All available", "")
        .initial_value(Window::Days(30))
        .interact()?;
    match window {
        Window::Today => select.today = true,
        Window::Days(n) => select.days = Some(n),
        Window::All => {
            // "All available" implies no session cap; the limit
            // prompt is skipped.
            select.all = true;
            return Ok(());
        }
    }
    let limit = cli::select("Session limit")
        .item(Some(20_usize), "20", "")
        .item(Some(50_usize), "50", "")
        .item(Some(100_usize), "100", "")
        .item(None, "All available", "")
        .initial_value(Some(20_usize))
        .interact()?;
    select.limit = limit;
    Ok(())
}

fn preview_dataset(plan: &DatasetPlan) -> Result<(), DialogError> {
    match plan {
        DatasetPlan::Reuse {
            id, session_count, ..
        } => {
            let line = format!(
                "dataset {} ({} {})",
                short_uuid(id),
                session_count,
                plural(*session_count, "session"),
            );
            cli::log::step(format!("Dataset\n{}", style(line).dim()))?;
        }
        DatasetPlan::New { selected } => {
            let n = selected.len();
            let line = format!("{n} {}", plural(n, "session"));
            cli::log::step(format!("Sessions\n{}", style(line).dim()))?;
        }
        DatasetPlan::None => {
            cli::log::step(format!("Dataset\n{}", style("none").dim()))?;
        }
    }
    Ok(())
}

/// Write the dataset if one needs minting, and return the commit
/// SHA that `ScanConfig.dataset` wants. `None` plan returns `None`;
/// `Reuse` returns its stored SHA.
fn materialize_dataset(plan: DatasetPlan, store: &Store) -> Result<Option<String>, DialogError> {
    match plan {
        DatasetPlan::None => Ok(None),
        DatasetPlan::Reuse { commit_sha, .. } => Ok(Some(commit_sha)),
        DatasetPlan::New { selected } => {
            if selected.is_empty() {
                return Err(DialogError::Failed(
                    "No sessions matched the selection".to_string(),
                ));
            }
            let id = cmd_dataset::create_dataset("gage scan2", store);
            cmd_dataset::add_native_to_dataset("gage scan2", store, &id, None, &selected, None);
            let record = DatasetStore::from(store)
                .get(&id)
                .map_err(|e| DialogError::Failed(format!("{e}")))?;
            Ok(Some(record.commit_sha))
        }
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
