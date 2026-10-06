use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Write as _};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use cliclack as cli;
use console::style;
use datafusion::arrow::array::{
    Array, BooleanArray, Int64Array, StringArray, TimestampMillisecondArray,
};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_claude::driver::ClaudeDriver;
use gage_claude::session::SessionInfo;
use gage_core::config::Config;
use gage_core::uuid::short_uuid;
use gage_query2::{ContextBuilder, ScanScope};
use gage_registry::driver::DriverRegistry;
use gage_registry::scanner::{
    Scanner, ScannerDef, ScannerRegistry, parse_scanner_file, split_scanner_spec,
};
use gage_runtime2::{LOG_TARGET, Output, TaskOutput};
use gage_scan2::scan_dir::scans_dir;
use gage_scan2::{CompiledScanner, Event, ScanConfig, ScanOutput, summary_line};
use gage_store::ScanDirLayout;
use gage_store::{
    DatasetStore, IssueStatus, IssueStore, SCAN_TYPE, ScanStore, StatusReason, Store,
};
use gage_tui::scan_view::{
    self, AgentItem, AgentState, EventItem, EvidenceItem, IssueItem, IssueSessionItem,
    IssueStatusUpdate, NoteItem, RunningTask, ScanCost, ScanHost, ScanLogs, ScanModel, ScanPickRow,
    ScanSetup, SessionCounts, SessionEntry, SessionItem, TaskAgent, TaskCost, TaskId, TaskItem,
    TaskState,
};
use gage_tui::session::Backend;
use gage_tui::text::fmt_duration;
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
    },
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::format::Writer;

use crate::cmd_dataset::{self, AddEvent};
use crate::cmd_issue2::event_label;
use crate::cmd_note::{count_rows, target_cell, value_cell};
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
    /// View a scan run
    ///
    /// Opens the scan's tasks, sessions, issues, and notes. Without a
    /// scan the view opens with a picker.
    View(Scan2ViewArgs),
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

    /// Don't show progress
    ///
    /// Task output and the scan's own lines print as they happen
    /// instead of the progress view. This is the output when stdout
    /// is not a terminal.
    #[arg(long, display_order = 16)]
    no_progress: bool,

    /// Show available scanners and exit
    #[arg(long, exclusive = true, display_order = 17)]
    list_scanners: bool,
}

#[derive(Args)]
pub struct Scan2ListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,
}

#[derive(Args)]
pub struct Scan2ViewArgs {
    /// Scan run ID (or prefix)
    scan: Option<String>,
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
        Some(Scan2Command::View(a)) => view(a).await,
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

async fn view(args: Scan2ViewArgs) {
    let store = Arc::new(Mutex::new(open_store("gage scan2 view")));
    let host = Arc::new(StoreHost {
        store,
        registry: DriverRegistry::builtin(),
        live: None,
    });
    // No scan arg: the view opens with its scan picker dialog.
    let model = match args.scan.as_deref() {
        Some(prefix) => match host.load_model(prefix).await {
            Ok(model) => Some(model),
            Err(e) => {
                eprintln!("gage scan2 view: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };
    if let Err(e) = scan_view::view(model, host).await {
        eprintln!("gage scan2 view: {e}");
        std::process::exit(1);
    }
}

/// The scan view's host over the store. `live` names the scan
/// directory of the scan being shown while it runs; the logs come
/// from there until the scan is applied and the directory removed.
struct StoreHost {
    store: Arc<Mutex<Store>>,
    registry: DriverRegistry,
    live: Option<ScanDirLayout>,
}

impl ScanHost for StoreHost {
    fn list_scans(&self) -> io::Result<Vec<ScanPickRow>> {
        block_on(self.list_rows()).map_err(|e| io::Error::other(e.to_string()))
    }

    fn load(&self, scan_id: &str) -> io::Result<ScanModel> {
        block_on(self.load_model(scan_id)).map_err(|e| io::Error::other(e.to_string()))
    }

    fn session_backend(&self) -> io::Result<Backend> {
        Ok(block_on(Backend::shared(Arc::clone(&self.store))))
    }

    fn read_logs(&self, log_key: &str) -> io::Result<ScanLogs> {
        if let Some(dir) = &self.live
            && dir.root().exists()
        {
            let logs_dir = dir.object_dir().join("logs");
            let read = |name: &str| match std::fs::read_to_string(logs_dir.join(name)) {
                Ok(content) => Ok(Some(content)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            };
            return Ok(ScanLogs {
                err: read("err")?,
                out: read("out")?,
                records: read("records")?,
            });
        }
        let store = self.lock();
        let scans = ScanStore::from(&*store);
        let commit = scans.get(log_key).map_err(io::Error::other)?.commit_sha;
        let read = |name: &str| {
            scans
                .scan_log(&commit, name)
                .map(|bytes| bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
                .map_err(io::Error::other)
        };
        Ok(ScanLogs {
            err: read("err")?,
            out: read("out")?,
            records: read("records")?,
        })
    }

    fn issue_status(&self, issue_id: &str) -> Result<IssueStatusUpdate, String> {
        let store = self.lock();
        let issue = IssueStore::from(&*store)
            .get(issue_id)
            .map_err(|e| e.to_string())?;
        let (status, status_cell) = status_cells(
            issue.status.as_str(),
            issue.status_reason.map(StatusReason::as_str),
        );
        Ok(IssueStatusUpdate {
            status,
            status_cell,
            closed: issue.status == IssueStatus::Closed,
            events: issue
                .changes
                .iter()
                .map(|c| EventItem {
                    kind: event_label(
                        c.event.as_str(),
                        c.from_status.map(IssueStatus::as_str),
                        c.to_status.map(IssueStatus::as_str),
                        c.reason.map(StatusReason::as_str),
                    ),
                    author: c.author.clone(),
                    timestamp: gage_core::datetime::ms_to_iso8601(c.timestamp_ms),
                    message: c.message.clone(),
                })
                .collect(),
        })
    }

    fn set_issue_status(
        &self,
        issue_id: &str,
        status: IssueStatus,
        reason: Option<StatusReason>,
        author: &str,
        message: Option<&str>,
    ) -> Result<(), String> {
        let store = self.lock();
        IssueStore::from(&*store)
            .set_status(issue_id, status, reason, author, message)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn add_issue_comment(&self, issue_id: &str, author: &str, message: &str) -> Result<(), String> {
        let store = self.lock();
        IssueStore::from(&*store)
            .comment(issue_id, author, message)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Run a future from the view's synchronous event loop, which lives
/// on a multi-threaded runtime.
fn block_on<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

impl StoreHost {
    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Every live scan, newest first, with its counts.
    async fn list_rows(&self) -> Result<Vec<ScanPickRow>, Box<dyn Error>> {
        let ctx = ContextBuilder::new(Some(Arc::clone(&self.store)))
            .build()
            .await;
        let sql = "SELECT s.id, s.id_display, s.tasks, s.canceled, s.started, s.stopped, \
                          s.created, ss.n, si.n, sn.n \
                   FROM scan s \
                   LEFT JOIN (SELECT scan_id, COUNT(*) AS n FROM scan_session GROUP BY scan_id) ss \
                        ON ss.scan_id = s.id \
                   LEFT JOIN (SELECT scan_id, COUNT(*) AS n FROM scan_issue GROUP BY scan_id) si \
                        ON si.scan_id = s.id \
                   LEFT JOIN (SELECT scan_id, COUNT(*) AS n FROM scan_note GROUP BY scan_id) sn \
                        ON sn.scan_id = s.id \
                   ORDER BY s.modified DESC";
        let mut rows = Vec::new();
        for batch in &query(&ctx, sql).await? {
            let ids = column::<StringArray>(batch, 0);
            let displays = column::<StringArray>(batch, 1);
            let tasks = column::<Int64Array>(batch, 2);
            let canceled = column::<BooleanArray>(batch, 3);
            let started = column::<TimestampMillisecondArray>(batch, 4);
            let stopped = column::<TimestampMillisecondArray>(batch, 5);
            let created = column::<TimestampMillisecondArray>(batch, 6);
            let sessions = column::<Int64Array>(batch, 7);
            let issues = column::<Int64Array>(batch, 8);
            let notes = column::<Int64Array>(batch, 9);
            for i in 0..batch.num_rows() {
                let count = |arr: &Int64Array| {
                    if arr.is_valid(i) {
                        arr.value(i) as usize
                    } else {
                        0
                    }
                };
                rows.push(ScanPickRow {
                    id: ids.value(i).to_string(),
                    id_display: displays.value(i).to_string(),
                    tasks: tasks.value(i) as usize,
                    sessions: count(sessions),
                    issues: count(issues),
                    notes: count(notes),
                    status: if canceled.value(i) {
                        "canceled"
                    } else {
                        "completed"
                    },
                    duration: elapsed(started, stopped, i),
                    label: String::new(),
                    created_ms: created.value(i),
                });
            }
        }
        Ok(rows)
    }

    /// The model of a stored scan, read through the scan's own scope:
    /// every table holds that scan's objects.
    async fn load_model(&self, prefix: &str) -> Result<ScanModel, Box<dyn Error>> {
        let id = {
            let store = self.lock();
            ScanStore::from(&*store).get(prefix)?.id
        };
        let ctx = self.scoped(ScanScope::stored(&id)).await;

        let (scan_elapsed, counts) = {
            let sql = "SELECT started, stopped, tasks, completed, failed, skipped FROM scan";
            let batches = query(&ctx, sql).await?;
            let batch = batches
                .iter()
                .find(|b| b.num_rows() > 0)
                .ok_or("scan row is missing")?;
            let started = column::<TimestampMillisecondArray>(batch, 0);
            let stopped = column::<TimestampMillisecondArray>(batch, 1);
            let n = |idx: usize| column::<Int64Array>(batch, idx).value(0) as usize;
            (elapsed(started, stopped, 0), (n(2), n(3) + n(4) + n(5)))
        };

        let results = self.load_results(&ctx).await?;
        let mut agents: HashMap<TaskId, Vec<AgentItem>> = HashMap::new();
        for ta in results.agents {
            agents.entry(ta.task).or_default().push(ta.agent);
        }
        let task_costs: HashMap<TaskId, ScanCost> = results
            .task_costs
            .into_iter()
            .map(|tc| (tc.id, tc.cost))
            .collect();

        let mut tasks = Vec::new();
        for batch in &query(
            &ctx,
            "SELECT scanner, task, status, started, stopped, worked_ms \
             FROM scan_task ORDER BY num",
        )
        .await?
        {
            let scanners = column::<StringArray>(batch, 0);
            let names = column::<StringArray>(batch, 1);
            let statuses = column::<StringArray>(batch, 2);
            let started = column::<TimestampMillisecondArray>(batch, 3);
            let stopped = column::<TimestampMillisecondArray>(batch, 4);
            let worked = column::<Int64Array>(batch, 5);
            for i in 0..batch.num_rows() {
                let id = TaskId {
                    scanner: scanners.value(i).to_string(),
                    task: names.value(i).to_string(),
                };
                let elapsed = if worked.is_valid(i) {
                    Some(Duration::from_millis(worked.value(i).max(0) as u64))
                } else {
                    elapsed(started, stopped, i)
                };
                tasks.push(TaskItem {
                    cost: task_costs.get(&id).copied(),
                    agents: agents.remove(&id).unwrap_or_default(),
                    state: match statuses.value(i) {
                        "pending" => TaskState::Pending,
                        "started" => TaskState::Running,
                        "completed" => TaskState::Completed,
                        "failed" => TaskState::Error,
                        "skipped" => TaskState::Skipped,
                        _ => TaskState::Canceled,
                    },
                    id,
                    elapsed,
                    progress: None,
                    pool_blocked: false,
                    worked: Duration::ZERO,
                    working_since: None,
                });
            }
        }
        let errors = tasks.iter().filter(|t| t.state == TaskState::Error).count();

        let counts_by_session: HashMap<String, (usize, usize)> = results
            .sessions
            .into_iter()
            .map(|c| (c.id, (c.notes, c.issues)))
            .collect();
        let mut sessions: Vec<SessionItem> = self
            .scan_sessions(&ctx)
            .await?
            .into_iter()
            .map(|entry| {
                let (notes, issues) = counts_by_session.get(&entry.id).copied().unwrap_or((0, 0));
                SessionItem {
                    id: entry.id,
                    title: entry.title,
                    path: None,
                    notes,
                    issues,
                }
            })
            .collect();
        sessions.sort_by(|a, b| {
            b.issues
                .cmp(&a.issues)
                .then_with(|| b.notes.cmp(&a.notes))
                .then_with(|| a.id.cmp(&b.id))
        });

        Ok(ScanModel {
            scan_id: short_uuid(&id).to_string(),
            label: None,
            log_key: Some(id),
            total: counts.0,
            progress: counts.1,
            notes: results.notes,
            issues: results.issues,
            cost: results.cost,
            errors,
            finished: true,
            elapsed: scan_elapsed,
            tasks,
            sessions,
        })
    }

    /// A context over one scan's objects, active or stored.
    async fn scoped(&self, scope: ScanScope) -> SessionContext {
        ContextBuilder::new(Some(Arc::clone(&self.store)))
            .scope(scope)
            .build()
            .await
    }

    /// The scan's sessions in member order, with titles.
    async fn scan_sessions(
        &self,
        ctx: &SessionContext,
    ) -> Result<Vec<SessionEntry>, Box<dyn Error>> {
        let mut sessions = Vec::new();
        for batch in &query(
            ctx,
            "SELECT m.session_id, s.title FROM scan_session m \
             JOIN session s ON s.id = m.session_id ORDER BY m.session_num",
        )
        .await?
        {
            let ids = column::<StringArray>(batch, 0);
            let titles = column::<StringArray>(batch, 1);
            for i in 0..batch.num_rows() {
                sessions.push(SessionEntry {
                    id: ids.value(i).to_string(),
                    title: string_or_empty(titles, i),
                    path: None,
                });
            }
        }
        Ok(sessions)
    }

    /// Notes, issues, per-session counts, agents, and costs of the
    /// scan `ctx` is scoped to: the stored loader's results and the
    /// live view's refresh. Agent session time bounds come from the
    /// store scope, since agent sessions are not dataset members.
    async fn load_results(&self, ctx: &SessionContext) -> Result<ScanResults, Box<dyn Error>> {
        let mut agents = Vec::new();
        let mut task_costs: HashMap<TaskId, (f64, bool)> = HashMap::new();
        let mut agent_ids = Vec::new();
        for batch in &query(
            ctx,
            "SELECT scanner, task, session_id, exit_code, result FROM scan_task_agent",
        )
        .await?
        {
            let scanners = column::<StringArray>(batch, 0);
            let tasks = column::<StringArray>(batch, 1);
            let sessions = column::<StringArray>(batch, 2);
            let exit_codes = column::<Int64Array>(batch, 3);
            let results = column::<StringArray>(batch, 4);
            for i in 0..batch.num_rows() {
                let task = TaskId {
                    scanner: scanners.value(i).to_string(),
                    task: tasks.value(i).to_string(),
                };
                let agent = agent_item(
                    sessions.value(i),
                    exit_codes.value(i),
                    results.is_valid(i).then(|| results.value(i)),
                );
                let cost = task_costs.entry(task.clone()).or_insert((0.0, false));
                match agent.cost {
                    Some(usd) => cost.0 += usd,
                    None => cost.1 = true,
                }
                agent_ids.push(agent.session_id.clone());
                agents.push(TaskAgent { task, agent });
            }
        }
        let times = self.agent_times(&agent_ids).await?;
        for ta in &mut agents {
            if let Some((started_ms, ended_ms)) = times.get(&ta.agent.session_id) {
                ta.agent.started_ms = Some(*started_ms);
                ta.agent.ended_ms = Some(*ended_ms);
            }
        }
        let cost = {
            let usd: f64 = task_costs.values().map(|(usd, _)| usd).sum();
            let incomplete = task_costs.values().any(|(_, incomplete)| *incomplete);
            (usd != 0.0 || incomplete).then_some(ScanCost { usd, incomplete })
        };
        let task_costs = task_costs
            .into_iter()
            .map(|(id, (usd, incomplete))| TaskCost {
                id,
                cost: ScanCost { usd, incomplete },
            })
            .collect();

        let mut notes = Vec::new();
        let mut note_sessions: HashMap<String, usize> = HashMap::new();
        for batch in &query(
            ctx,
            "SELECT id, name, value, text, author, target, metadata, created \
             FROM note ORDER BY created",
        )
        .await?
        {
            let ids = column::<StringArray>(batch, 0);
            let names = column::<StringArray>(batch, 1);
            let values = column::<StringArray>(batch, 2);
            let texts = column::<StringArray>(batch, 3);
            let authors = column::<StringArray>(batch, 4);
            let targets = column::<StringArray>(batch, 5);
            let metadatas = column::<StringArray>(batch, 6);
            let createds = column::<TimestampMillisecondArray>(batch, 7);
            for i in 0..batch.num_rows() {
                let value_full = note_value(values, texts, i);
                let target = string_or_empty(targets, i);
                if let Some(session) = target_session(&target) {
                    *note_sessions.entry(session).or_default() += 1;
                }
                notes.push(NoteItem {
                    id: ids.value(i).to_string(),
                    name: names.value(i).to_string(),
                    value: value_cell(&value_full),
                    value_full,
                    target_cell: target_cell(&target),
                    target,
                    author: authors.value(i).to_string(),
                    created: timestamp_display(createds, i),
                    metadata: metadatas
                        .is_valid(i)
                        .then(|| metadatas.value(i).to_string()),
                });
            }
        }

        let mut evidence: HashMap<String, Vec<EvidenceItem>> = HashMap::new();
        for batch in &query(
            ctx,
            "SELECT e.issue_id, n.id, n.name, n.target, n.value, n.text \
             FROM issue_evidence e JOIN note n ON n.id = e.note_id",
        )
        .await?
        {
            let issues = column::<StringArray>(batch, 0);
            let ids = column::<StringArray>(batch, 1);
            let names = column::<StringArray>(batch, 2);
            let targets = column::<StringArray>(batch, 3);
            let values = column::<StringArray>(batch, 4);
            let texts = column::<StringArray>(batch, 5);
            for i in 0..batch.num_rows() {
                evidence
                    .entry(issues.value(i).to_string())
                    .or_default()
                    .push(EvidenceItem {
                        id: ids.value(i).to_string(),
                        name: names.value(i).to_string(),
                        target: string_or_empty(targets, i),
                        value: note_value(values, texts, i),
                    });
            }
        }

        let mut issue_sessions: HashMap<String, Vec<IssueSessionItem>> = HashMap::new();
        let mut session_issues: HashMap<String, usize> = HashMap::new();
        for batch in &query(
            ctx,
            "SELECT i.issue_id, i.session_id, s.project, s.driver, s.title \
             FROM session_issue i JOIN session s ON s.id = i.session_id",
        )
        .await?
        {
            let issues = column::<StringArray>(batch, 0);
            let sessions = column::<StringArray>(batch, 1);
            let projects = column::<StringArray>(batch, 2);
            let drivers = column::<StringArray>(batch, 3);
            let titles = column::<StringArray>(batch, 4);
            for i in 0..batch.num_rows() {
                let session = sessions.value(i).to_string();
                *session_issues.entry(session.clone()).or_default() += 1;
                issue_sessions
                    .entry(issues.value(i).to_string())
                    .or_default()
                    .push(IssueSessionItem {
                        project: self.project_display(drivers.value(i), projects, i),
                        title: string_or_empty(titles, i),
                        id: session,
                    });
            }
        }

        let mut events: HashMap<String, Vec<EventItem>> = HashMap::new();
        for batch in &query(
            ctx,
            "SELECT issue_id, timestamp, author, event, from_status, to_status, reason, message \
             FROM issue_event ORDER BY timestamp",
        )
        .await?
        {
            let issues = column::<StringArray>(batch, 0);
            let timestamps = column::<TimestampMillisecondArray>(batch, 1);
            let authors = column::<StringArray>(batch, 2);
            let kinds = column::<StringArray>(batch, 3);
            let froms = column::<StringArray>(batch, 4);
            let tos = column::<StringArray>(batch, 5);
            let reasons = column::<StringArray>(batch, 6);
            let messages = column::<StringArray>(batch, 7);
            for i in 0..batch.num_rows() {
                let opt = |arr: &StringArray| arr.is_valid(i).then(|| arr.value(i).to_string());
                events
                    .entry(issues.value(i).to_string())
                    .or_default()
                    .push(EventItem {
                        kind: event_label(
                            kinds.value(i),
                            opt(froms).as_deref(),
                            opt(tos).as_deref(),
                            opt(reasons).as_deref(),
                        ),
                        author: authors.value(i).to_string(),
                        timestamp: timestamp_display(timestamps, i),
                        message: opt(messages),
                    });
            }
        }

        let mut issues = Vec::new();
        for batch in &query(
            ctx,
            "SELECT id, name, title, description, status, status_reason, author, created \
             FROM issue ORDER BY created",
        )
        .await?
        {
            let ids = column::<StringArray>(batch, 0);
            let names = column::<StringArray>(batch, 1);
            let titles = column::<StringArray>(batch, 2);
            let descriptions = column::<StringArray>(batch, 3);
            let statuses = column::<StringArray>(batch, 4);
            let reasons = column::<StringArray>(batch, 5);
            let authors = column::<StringArray>(batch, 6);
            let createds = column::<TimestampMillisecondArray>(batch, 7);
            for i in 0..batch.num_rows() {
                let id = ids.value(i).to_string();
                let (status, status_cell) = status_cells(
                    statuses.value(i),
                    reasons.is_valid(i).then(|| reasons.value(i)),
                );
                issues.push(IssueItem {
                    name: names.value(i).to_string(),
                    title: titles.value(i).lines().next().unwrap_or("").to_string(),
                    status,
                    status_cell,
                    closed: statuses.value(i) == IssueStatus::Closed.as_str(),
                    author: authors.value(i).to_string(),
                    created: timestamp_display(createds, i),
                    description: descriptions
                        .is_valid(i)
                        .then(|| descriptions.value(i).to_string()),
                    sessions: issue_sessions.remove(&id).unwrap_or_default(),
                    evidence: evidence.remove(&id).unwrap_or_default(),
                    events: events.remove(&id).unwrap_or_default(),
                    id,
                });
            }
        }

        let mut session_ids: Vec<&String> =
            note_sessions.keys().chain(session_issues.keys()).collect();
        session_ids.sort();
        session_ids.dedup();
        let sessions = session_ids
            .into_iter()
            .map(|id| SessionCounts {
                id: id.clone(),
                notes: note_sessions.get(id).copied().unwrap_or(0),
                issues: session_issues.get(id).copied().unwrap_or(0),
            })
            .collect();

        Ok(ScanResults {
            notes,
            issues,
            sessions,
            cost,
            task_costs,
            agents,
        })
    }

    /// First and last message timestamps per agent session, from the
    /// store scope.
    async fn agent_times(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, (i64, i64)>, Box<dyn Error>> {
        let mut times = HashMap::new();
        if ids.is_empty() {
            return Ok(times);
        }
        let ctx = ContextBuilder::new(Some(Arc::clone(&self.store)))
            .build()
            .await;
        let in_list = ids
            .iter()
            .map(|id| format!("'{}'", id.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT session_id, MIN(timestamp), MAX(timestamp) FROM message \
             WHERE session_id IN ({in_list}) GROUP BY session_id"
        );
        for batch in &query(&ctx, &sql).await? {
            let sessions = column::<StringArray>(batch, 0);
            let firsts = column::<TimestampMillisecondArray>(batch, 1);
            let lasts = column::<TimestampMillisecondArray>(batch, 2);
            for i in 0..batch.num_rows() {
                if firsts.is_valid(i) && lasts.is_valid(i) {
                    times.insert(
                        sessions.value(i).to_string(),
                        (firsts.value(i), lasts.value(i)),
                    );
                }
            }
        }
        Ok(times)
    }

    /// A session's project as its driver displays it; the stored
    /// name when the driver is unknown.
    fn project_display(&self, driver: &str, projects: &StringArray, i: usize) -> String {
        if !projects.is_valid(i) {
            return String::new();
        }
        let name = driver.split_once(' ').map_or(driver, |(name, _)| name);
        match self.registry.for_name(name) {
            Some(driver) => driver.format_project(projects.value(i), 60),
            None => projects.value(i).to_string(),
        }
    }
}

/// Notes, issues, per-session counts, agents, and costs of a scan,
/// as the stored loader and the live refresh read them.
struct ScanResults {
    notes: Vec<NoteItem>,
    issues: Vec<IssueItem>,
    sessions: Vec<SessionCounts>,
    cost: Option<ScanCost>,
    task_costs: Vec<TaskCost>,
    agents: Vec<TaskAgent>,
}

impl ScanResults {
    fn into_event(self) -> scan_view::Event {
        scan_view::Event::Results {
            notes: self.notes,
            issues: self.issues,
            sessions: self.sessions,
            cost: self.cost,
            task_costs: self.task_costs,
            agents: self.agents,
        }
    }
}

/// Show the progress view over a running scan. Runner events arrive
/// on `events`; the first, `Started`, names the scan and its tasks
/// and is what the view's model is built from. Task starts, stops,
/// and progress become status snapshots; task output becomes log
/// lines; notes, issues, and agents are re-read from the scan
/// directory once a second and from the store once the scan is
/// applied. Closing the view mid-scan cancels the run.
async fn drive_view(
    mut events: UnboundedReceiver<Event>,
    store: Arc<Mutex<Store>>,
    cancel: CancellationToken,
) -> io::Result<()> {
    let Some(Event::Started { id, tasks }) = events.recv().await else {
        // The runner failed before planning; the caller reports it
        return Ok(());
    };
    let layout = ScanDirLayout::new(scans_dir().join(&id));
    let host = Arc::new(StoreHost {
        store,
        registry: DriverRegistry::builtin(),
        live: Some(layout.clone()),
    });
    let sessions = {
        let ctx = host.scoped(ScanScope::scan_dir(layout.root())).await;
        host.scan_sessions(&ctx)
            .await
            .map_err(|e| io::Error::other(format!("reading the scan's sessions: {e}")))?
    };
    let task_ids: Vec<TaskId> = tasks
        .into_iter()
        .map(|(scanner, task)| TaskId { scanner, task })
        .collect();
    let mut model = ScanModel::new(ScanSetup {
        tasks: task_ids,
        sessions,
    });
    model.scan_id = short_uuid(&id).to_string();
    model.log_key = Some(id.clone());

    let (view_tx, view_rx) = unbounded_channel();
    let done = Arc::new(AtomicBool::new(false));
    let poll = tokio::spawn({
        let host = Arc::clone(&host);
        let tx = view_tx.clone();
        let done = Arc::clone(&done);
        let root = layout.root().to_path_buf();
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if done.load(Ordering::Relaxed) {
                    return;
                }
                let ctx = host.scoped(ScanScope::scan_dir(&root)).await;
                let event = match host.load_results(&ctx).await {
                    Ok(results) => results.into_event(),
                    Err(e) => scan_view::Event::Log(format!("results refresh failed: {e}")),
                };
                send_view_event(&tx, event);
            }
        }
    });

    let total = model.total;
    let forward_host = Arc::clone(&host);
    let forward = async {
        let mut status = LiveStatus::new(total);
        while let Some(event) = events.recv().await {
            match event {
                Event::Started { .. } => {}
                Event::TaskStarted { scanner, task } => {
                    status.start(TaskId { scanner, task });
                    send_view_event(&view_tx, status.snapshot());
                }
                Event::TaskFinished {
                    scanner,
                    task,
                    status: task_status,
                    error,
                } => {
                    let id = TaskId { scanner, task };
                    if task_status == gage_store::TaskStatus::Failed {
                        send_view_event(
                            &view_tx,
                            scan_view::Event::Failed {
                                scanner: id.scanner.clone(),
                                task: id.task.clone(),
                                message: error.unwrap_or_default(),
                            },
                        );
                    }
                    status.finish(&id);
                    send_view_event(&view_tx, status.snapshot());
                }
                Event::Output(TaskOutput {
                    scanner,
                    task,
                    output,
                }) => match output {
                    Output::Progress { pos, total } => {
                        status.progress(&TaskId { scanner, task }, pos, total);
                        send_view_event(&view_tx, status.snapshot());
                    }
                    Output::Print(text) | Output::Println(text) => send_view_event(
                        &view_tx,
                        scan_view::Event::Log(format!("{scanner}:{task}: {text}")),
                    ),
                    // Log records are in the scan's `records`, shown by `l`
                    Output::Log { .. } => {}
                },
                Event::Scan(ScanOutput::Out(text) | ScanOutput::Err(text)) => {
                    send_view_event(&view_tx, scan_view::Event::Log(text.trim_end().to_string()));
                }
                Event::Warning {
                    scanner,
                    task,
                    message,
                } => send_view_event(
                    &view_tx,
                    scan_view::Event::Warning {
                        scanner,
                        task,
                        message,
                    },
                ),
                // Every task is terminal; the apply that follows moves
                // the scan directory into the store, so the poll stops
                // reading it
                Event::Summary { .. } => done.store(true, Ordering::Relaxed),
            }
        }
        // The runner returned: the scan is applied, or it failed. One
        // final read from the store makes the view current.
        done.store(true, Ordering::Relaxed);
        poll.abort();
        if let Err(e) = poll.await
            && !e.is_cancelled()
        {
            panic!("results poll joined cleanly: {e}");
        }
        let ctx = forward_host.scoped(ScanScope::stored(&id)).await;
        let event = match forward_host.load_results(&ctx).await {
            Ok(results) => results.into_event(),
            Err(e) => scan_view::Event::Log(format!("results refresh failed: {e}")),
        };
        send_view_event(&view_tx, event);
        send_view_event(&view_tx, scan_view::Event::Finished);
    };
    let run_cancel = cancel.clone();
    let view = scan_view::run(model, view_rx, move || run_cancel.cancel(), host);
    let ((), view_result) = tokio::join!(forward, view);
    // Closing the view mid-scan stops the run; after the scan
    // completes this is a no-op
    cancel.cancel();
    view_result.map_err(|e| io::Error::other(format!("progress view: {e}")))
}

/// The tasks on workers and the counts the view's status snapshot
/// carries, kept in step with the runner's events.
struct LiveStatus {
    total: usize,
    finished: usize,
    running: Vec<RunningTask>,
}

impl LiveStatus {
    fn new(total: usize) -> Self {
        Self {
            total,
            finished: 0,
            running: Vec::new(),
        }
    }

    fn start(&mut self, id: TaskId) {
        self.running.push(RunningTask {
            id,
            progress: None,
            pool_blocked: false,
            worked: Duration::ZERO,
            working_since: Some(Instant::now()),
        });
    }

    fn finish(&mut self, id: &TaskId) {
        self.running.retain(|r| r.id != *id);
        self.finished += 1;
    }

    fn progress(&mut self, id: &TaskId, pos: u64, total: u64) {
        if let Some(task) = self.running.iter_mut().find(|r| r.id == *id) {
            task.progress = Some((pos, total));
        }
    }

    fn snapshot(&self) -> scan_view::Event {
        scan_view::Event::Status {
            total: self.total,
            progress: self.finished,
            running: self.running.clone(),
        }
    }
}

/// Deliver a runner event to the view bridge. A send fails only once
/// the bridge has dropped its receiver, which happens after the view
/// has closed; an event then has no reader.
fn send_runner_event(tx: &UnboundedSender<Event>, event: Event) {
    if tx.send(event).is_err() {}
}

/// Deliver an event to the view. A send fails only once the view has
/// closed its receiver, after which an event has no reader.
fn send_view_event(tx: &UnboundedSender<scan_view::Event>, event: scan_view::Event) {
    if tx.send(event).is_err() {}
}

/// An agent's view entry from its record: state and cost from the
/// harness result, error from the exit code when there is no result.
fn agent_item(session_id: &str, exit_code: i64, result: Option<&str>) -> AgentItem {
    let result = result.and_then(|raw| match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(session_id, "unparseable agent result: {e}");
            None
        }
    });
    let state = match &result {
        Some(r) => {
            if r.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) {
                AgentState::Error
            } else {
                AgentState::Done
            }
        }
        None if exit_code == 0 => AgentState::Done,
        None => AgentState::Error,
    };
    AgentItem {
        session_id: session_id.to_string(),
        path: None,
        state,
        cost: result
            .as_ref()
            .and_then(|r| r.get("total_cost_usd"))
            .and_then(serde_json::Value::as_f64),
        started_ms: None,
        ended_ms: None,
    }
}

/// Status display strings: the long form `closed (completed)` and
/// the table cell, which is the reason when there is one.
fn status_cells(status: &str, reason: Option<&str>) -> (String, String) {
    match reason {
        Some(r) => (format!("{status} ({r})"), r.to_string()),
        None => (status.to_string(), status.to_string()),
    }
}

/// A note's value text: the text value, or the JSON value pretty
/// printed.
fn note_value(values: &StringArray, texts: &StringArray, i: usize) -> String {
    if texts.is_valid(i) {
        return texts.value(i).to_string();
    }
    match serde_json::from_str::<serde_json::Value>(values.value(i)) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap(),
        Err(_) => values.value(i).to_string(),
    }
}

/// The session id a `session:` target URL names
fn target_session(target: &str) -> Option<String> {
    let parsed = gage_store::url::parse(target).ok()?;
    (parsed.scheme == "session").then(|| parsed.body.to_string())
}

fn elapsed(
    started: &TimestampMillisecondArray,
    stopped: &TimestampMillisecondArray,
    i: usize,
) -> Option<Duration> {
    (started.is_valid(i) && stopped.is_valid(i)).then(|| {
        Duration::from_millis(stopped.value(i).saturating_sub(started.value(i)).max(0) as u64)
    })
}

fn timestamp_display(col: &TimestampMillisecondArray, i: usize) -> String {
    if col.is_valid(i) {
        gage_core::datetime::ms_to_iso8601(col.value(i))
    } else {
        String::new()
    }
}

fn string_or_empty(col: &StringArray, i: usize) -> String {
    if col.is_valid(i) {
        col.value(i).to_string()
    } else {
        String::new()
    }
}

async fn query(ctx: &SessionContext, sql: &str) -> Result<Vec<RecordBatch>, Box<dyn Error>> {
    Ok(ctx.sql(sql).await?.collect().await?)
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

    // The dialog settles the inputs and builds the dataset; the scan
    // itself runs after it, so its output lands outside the dialog
    let mut prep: Option<ScanPrep> = None;
    dialog::run_async("Scan sessions", || {
        prepare_scan_dialog(args, &registry, &config, &store, &mut prep)
    })
    .await;
    // A declined confirmation leaves nothing to run; a failure has
    // already exited
    let Some(prep) = prep else {
        return;
    };
    run_prepared(prep, &store).await;
}

/// Everything the dialog settled for the run.
struct ScanPrep {
    compiled: Vec<CompiledScanner>,
    /// The dataset to scan, built or reused once the run starts
    plan: DatasetPlan,
    jobs: usize,
    invalidate: bool,
    /// The progress view is shown; headless otherwise
    progress_ui: bool,
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

async fn prepare_scan_dialog(
    mut args: Scan2RunArgs,
    registry: &ScannerRegistry,
    config: &Config,
    store: &Store,
    prep: &mut Option<ScanPrep>,
) -> Result<dialog::DialogResult, DialogError> {
    // Scanner selection: -s/-g/-f, the `default` group under -y, or
    // an interactive multi-select when the user supplied nothing.
    let (scanner_specs, prompted) = resolve_scanner_specs(&args, registry, config)?;
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
    let plan = resolve_dataset_plan(&mut args, store).await?;
    preview_dataset(&plan)?;

    // Preflight: compile every selected scanner and every pulled-in
    // required_by dependent before any task runs.
    let defs: Vec<&ScannerDef> = scanners.iter().map(|s| s.def).collect();
    let required = if args.no_deps {
        Vec::new()
    } else {
        registry.required_tasks(&defs, config)
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

    if let DatasetPlan::New { selected } = &plan
        && selected.is_empty()
    {
        return Err(DialogError::Failed(
            "No sessions matched the selection".to_string(),
        ));
    }
    *prep = Some(ScanPrep {
        compiled,
        plan,
        jobs: args.jobs,
        invalidate: args.invalidate,
        progress_ui: !args.no_progress && io::stdout().is_terminal(),
    });
    Ok("Starting scan".into())
}

/// Build the dataset, then run the prepared scan, headless or under
/// the progress view, and print its summary line. A canceled scan or
/// one with a failed task exits with status 1.
async fn run_prepared(prep: ScanPrep, store: &Store) {
    let dataset_sha = match materialize_dataset(prep.plan, store, prep.progress_ui) {
        Ok(sha) => sha,
        Err(e) => {
            eprintln!("gage scan2: {e}");
            std::process::exit(1);
        }
    };

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
        jobs: prep.jobs,
        driver: Arc::new(ClaudeDriver::new()),
        invalidate: prep.invalidate,
    };
    let result = if !prep.progress_ui {
        // Headless: task output and the scan's own lines go to the
        // terminal as they happen, unprefixed, with a status line for
        // the start of the scan and of each task, and each task's end
        let mut task_starts: HashMap<String, Instant> = HashMap::new();
        gage_scan2::scan(
            store,
            &scan_config,
            &prep.compiled,
            &cancel,
            |event| match event {
                Event::Output(TaskOutput { output, .. }) => match output {
                    Output::Print(s) => print!("{s}"),
                    Output::Println(s) => println!("{s}"),
                    Output::Log { .. } | Output::Progress { .. } => {}
                },
                Event::Scan(ScanOutput::Out(s)) => print!("{s}"),
                Event::Scan(ScanOutput::Err(s)) => eprint!("{s}"),
                Event::Started { id, tasks } => {
                    let n = tasks.len();
                    println!(
                        "Scan {} started: {n} {}",
                        short_uuid(&id),
                        plural(n, "task")
                    );
                }
                Event::Summary { .. } => {}
                Event::Warning {
                    scanner,
                    task,
                    message,
                } => eprintln!("warning: task {scanner}:{task} {message}"),
                Event::TaskStarted { scanner, task } => {
                    task_starts.insert(format!("{scanner}:{task}"), Instant::now());
                    println!("Task {scanner}:{task} started");
                }
                Event::TaskFinished {
                    scanner,
                    task,
                    status,
                    ..
                } => {
                    let label = format!("{scanner}:{task}");
                    match task_starts.remove(&label) {
                        Some(started) => println!(
                            "Task {label} {} in {}",
                            status.as_str(),
                            fmt_duration(started.elapsed())
                        ),
                        None => println!("Task {label} {}", status.as_str()),
                    }
                }
            },
        )
        .await
    } else {
        // The progress view reads the scan through its own store
        // handle: the runner's stays free for the tasks and the apply
        let view_store = match Store::open(&gage_store::store_path()) {
            Ok(store) => store,
            Err(e) => {
                eprintln!("gage scan2: {e}");
                std::process::exit(1);
            }
        };
        let (tx, rx) = unbounded_channel();
        let scan_fut =
            gage_scan2::scan(store, &scan_config, &prep.compiled, &cancel, move |event| {
                send_runner_event(&tx, event)
            });
        let view_fut = drive_view(rx, Arc::new(Mutex::new(view_store)), cancel.clone());
        let (result, view_result) = tokio::join!(scan_fut, view_fut);
        if let Err(e) = view_result {
            eprintln!("gage scan2: {e}");
            std::process::exit(1);
        }
        result
    };
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
    let shown_id = styled_scan_id(store, &outcome.id);
    println!("{}", summary_line(&shown_id, &outcome.attrs));
    if outcome.attrs.canceled || outcome.attrs.tasks.failed > 0 {
        std::process::exit(1);
    }
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
/// The commit of the dataset to scan: the reused dataset's, or that
/// of a new dataset built from the selected sessions. The build shows
/// a progress bar under `progress`, cleared when it completes, and
/// prints each session's outcome line otherwise; the closing `Added
/// N sessions` line prints either way.
fn materialize_dataset(
    plan: DatasetPlan,
    store: &Store,
    progress: bool,
) -> Result<Option<String>, String> {
    match plan {
        DatasetPlan::None => Ok(None),
        DatasetPlan::Reuse { commit_sha, .. } => Ok(Some(commit_sha)),
        DatasetPlan::New { selected } => {
            let id = cmd_dataset::create_dataset("gage scan2", store);
            let mut bar: Option<indicatif::ProgressBar> = None;
            cmd_dataset::add_native_to_dataset(store, &id, None, &selected, None, &mut |event| {
                match &event {
                    AddEvent::Starting { total, .. } => {
                        if progress {
                            bar.get_or_insert_with(|| add_progress_bar(*total))
                                .set_message(event.to_string());
                        }
                    }
                    AddEvent::Added { .. } => match &bar {
                        Some(bar) => bar.inc(1),
                        None => println!("{event}"),
                    },
                    AddEvent::Done { .. } => {
                        if let Some(bar) = bar.take() {
                            bar.finish_and_clear();
                        }
                        println!("{event}");
                    }
                }
            })?;
            Ok(Some(
                DatasetStore::from(store)
                    .get(&id)
                    .map_err(|e| e.to_string())?
                    .commit_sha,
            ))
        }
    }
}

/// The dataset build's progress bar, styled like the CLI's spinner
fn add_progress_bar(total: usize) -> indicatif::ProgressBar {
    let bar = indicatif::ProgressBar::new(total as u64);
    bar.set_style(
        indicatif::ProgressStyle::with_template("{spinner:.magenta}  {msg} {bar:30} {pos}/{len}")
            .unwrap(),
    );
    bar.enable_steady_tick(Duration::from_millis(80));
    bar
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
