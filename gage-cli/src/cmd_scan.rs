use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Subcommand};
use cliclack as cli;
use console::style;
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
        peaker::Priority,
    },
};

use gage_claude::home::ClaudeHome;
use gage_claude::project::project_display;
use gage_claude::session::{self, SessionInfo, SessionListBuilder};
use gage_core::task::task_display;
use gage_core::uuid::short_uuid;
use gage_db::{db, scan};
use gage_query::ScanSessionContext;
use gage_registry::scanner::{Scanner, ScannerRegistry};
use rand::seq::SliceRandom;

use crate::dialog::{self, DialogError, DialogResult};
use crate::style as s;

const DEFAULT_AGENT_JOBS: usize = 8;

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ScanArgs {
    /// Operate on agent sessions instead of Claude Code sessions
    #[arg(short = 'A', long)]
    pub agent: bool,

    #[command(subcommand)]
    pub command: Option<ScanCommand>,

    #[command(flatten)]
    pub run_args: ScanRunArgs,
}

#[derive(Subcommand)]
pub enum ScanCommand {
    /// List scan runs
    List(ScanListArgs),

    /// View a scan run in the scan TUI
    View(ScanViewArgs),

    /// Delete scan runs and associated notes
    Delete(ScanDeleteArgs),

    /// Invalidate task validation state
    ///
    /// Invalidated tasks are re-run on the next applicable scan.
    Invalidate(ScanInvalidateArgs),
}

#[derive(Args)]
pub struct ScanViewArgs {
    /// Scan ID (or prefix)
    scan_id: Option<String>,
}

#[derive(Args)]
pub struct ScanListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,
}

#[derive(Args)]
pub struct ScanDeleteArgs {
    /// Scan run IDs (or prefix)
    ids: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
#[command(group = clap::ArgGroup::new("target").required(true).multiple(true))]
pub struct ScanInvalidateArgs {
    /// Invalidate tasks for a session (ID or prefix, repeatable)
    #[arg(short, long = "session", value_name = "ID", group = "target")]
    sessions: Vec<String>,

    /// Invalidate tasks for a note (ID or prefix, repeatable)
    #[arg(short, long = "note", value_name = "ID", group = "target")]
    notes: Vec<String>,

    /// Invalidate tasks by name (or prefix, repeatable)
    #[arg(short, long = "task", value_name = "NAME", group = "target")]
    tasks: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct ScanRunArgs {
    /// Session IDs to scan (or prefix)
    #[arg(value_name = "SESSION", conflicts_with_all = ["limit", "days", "today", "all", "sample", "project"])]
    sessions: Vec<String>,

    /// Limit sessions to a project
    ///
    /// PROJECT is a project directory path (absolute, relative, or
    /// ~-prefixed) or a project slug as shown by 'gage session list'.
    #[arg(short, long, value_name = "PROJECT", allow_hyphen_values = true, conflicts_with_all = ["rerun", "scan"])]
    project: Option<String>,

    /// Scanner to run (repeatable)
    #[arg(short, long = "scanner", value_name = "NAME")]
    scanners: Vec<String>,

    /// Run the scanners in a group (repeatable)
    #[arg(short, long = "group", value_name = "NAME")]
    groups: Vec<String>,

    /// Scanner file to run (repeatable)
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    files: Vec<String>,

    /// Scan the latest N sessions
    ///
    /// Combines with --days or --today to cap the sessions selected
    /// from the window.
    #[arg(short = 'n', long, value_name = "N", conflicts_with_all = ["all", "sample"])]
    limit: Option<usize>,

    /// Scan N sessions selected at random
    ///
    /// Samples from sessions modified in the past 30 days, or the
    /// window given with --days or --today.
    #[arg(short = 'r', long, value_name = "N", conflicts_with = "all")]
    sample: Option<usize>,

    /// Scan sessions from past N days
    #[arg(short, long, value_name = "N", conflicts_with = "all")]
    days: Option<u32>,

    /// Scan sessions from today
    ///
    /// Selects sessions modified since midnight local time.
    #[arg(short, long, conflicts_with_all = ["days", "all"])]
    today: bool,

    /// Scan all sessions
    #[arg(short, long)]
    all: bool,

    /// Re-run a scan's scanners on its sessions
    ///
    /// SCAN is a scan ID or prefix. Use 'gage scan list' to show scan
    /// runs.
    #[arg(
        long,
        value_name = "SCAN",
        conflicts_with_all = ["sessions", "scanners", "files", "limit", "days", "today", "all", "sample"]
    )]
    rerun: Option<String>,

    /// Scan a scan's agent sessions
    ///
    /// SCAN is a scan ID or prefix. Expands to the agent sessions the
    /// scan's tasks spawned and selects the `eval` scanner group.
    /// --limit and --sample cap the expanded list.
    #[arg(
        long,
        value_name = "SCAN",
        conflicts_with_all = ["sessions", "scanners", "files", "groups", "rerun", "days", "today", "all"]
    )]
    scan: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    /// Maximum concurrent tasks (defaults to number of CPUs)
    #[arg(short, long, value_name = "N")]
    jobs: Option<usize>,

    /// Maximum concurrent agents (default 8)
    #[arg(long, value_name = "N")]
    agent_jobs: Option<usize>,

    /// Don't show progress
    #[arg(long)]
    no_progress: bool,

    /// Show available scanners and exit
    #[arg(long)]
    list_scanners: bool,
}

pub async fn run(args: ScanArgs) {
    match args.command {
        Some(ScanCommand::List(a)) => list(a),
        Some(ScanCommand::View(a)) => view(a).await,
        Some(ScanCommand::Delete(a)) => delete(a),
        Some(ScanCommand::Invalidate(a)) => invalidate(a),
        None => run_scan(args.run_args).await,
    }
}

fn list(args: ScanListArgs) {
    let conn = db::open_db().unwrap();
    let runs = match scan::all(&conn) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let total = runs.len();
    if total == 0 {
        println!("No scan runs found");
        return;
    }

    let show = args.limit.show_count(total);

    let highlighter = s::IdHighlighter::new(match scan::all_ids(&conn) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    });

    let header: Vec<String> = [
        "Id", "Tasks", "Sessions", "Notes", "Issues", "Errors", "Cost", "Status", "Duration",
        "Created",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let mut rows: Vec<Vec<String>> = Vec::with_capacity(show);
    for run in runs.iter().take(show) {
        match list_row(&conn, run, &highlighter) {
            Ok(row) => rows.push(row),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }

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

    args.limit.print_summary(show, total, "scan run");
}

fn list_row(
    conn: &gage_db::rusqlite::Connection,
    run: &scan::Scan,
    highlighter: &s::IdHighlighter,
) -> anyhow::Result<Vec<String>> {
    let tasks = scan::tasks_for_scan(conn, &run.id)?;
    let errors = tasks
        .iter()
        .filter(|t| t.status == scan::TaskStatus::Failed)
        .count();
    let sessions = scan::session_ids_for_scan(conn, &run.id)?.len();
    let notes = scan::note_ids_for_scan(conn, &run.id)?.len();
    let issues = scan::issue_ids_for_scan(conn, &run.id)?.len();
    let cost = format_cost(scan::cost_for_scan(conn, &run.id)?);
    let metadata = run.parse_metadata()?;
    // A Running payload whose process is gone is a run that died;
    // absent metadata (a run predating the running marker) reads the
    // same. Both render as incomplete.
    let status = match &metadata {
        Some(scan::ScanMetadata::Scan(s)) if s.canceled => "canceled",
        Some(scan::ScanMetadata::Running(r)) if scan_process_alive(r.pid, run.created) => "running",
        Some(scan::ScanMetadata::Running(_)) | None => "incomplete",
        Some(_) => "completed",
    };
    let duration = metadata
        .and_then(|m| m.elapsed_ms())
        .map(|ms| crate::human::format_duration(Duration::from_millis(ms)))
        .unwrap_or_default();
    Ok(vec![
        highlighter.short(&run.id),
        tasks.len().to_string(),
        sessions.to_string(),
        notes.to_string(),
        issues.to_string(),
        errors.to_string(),
        cost,
        status.to_string(),
        duration,
        crate::human::format_elapsed_ms(run.created),
    ])
}

/// USD cost as `$X.XX`, with a trailing `+` when any agent's cost is
/// unrecorded (the total understates true spend). Empty when the scan
/// has no recorded agents.
fn format_cost(cost: scan::ScanCost) -> String {
    if cost.total_usd == 0.0 && !cost.incomplete {
        return String::new();
    }
    let suffix = if cost.incomplete { "+" } else { "" };
    format!("${:.2}{suffix}", cost.total_usd)
}

/// Whether `pid` is alive and is the process that recorded it in the
/// scan's Running metadata. Existence alone is not identity — a pid
/// can be recycled. A recycled holder necessarily started after the
/// original process died, and the original started before the scan
/// row was created; so a process whose start postdates `created_ms`
/// is disqualified. Unreadable process data reads as not running.
fn scan_process_alive(pid: u32, created_ms: i64) -> bool {
    // btime and the tick conversion each truncate to whole seconds;
    // the slack keeps a genuine process from reading as started just
    // after its own scan row
    const SLACK_SECS: i64 = 2;
    match process_start_epoch(pid) {
        Some(start_secs) => start_secs <= created_ms / 1000 + SLACK_SECS,
        None => false,
    }
}

/// A process's start time in epoch seconds: the boot time (`btime` in
/// `/proc/stat`) plus its start offset (field 22 of `/proc/{pid}/stat`,
/// in clock ticks since boot). None when the process is gone or its
/// stat data cannot be read.
fn process_start_epoch(pid: u32) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) may contain spaces; fields resume after its
    // closing paren with state (field 3), putting starttime (field 22)
    // at offset 19
    let after_comm = stat.rsplit(')').next()?;
    let ticks: i64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;
    let btime: i64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse().ok())?;
    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_sec <= 0 {
        return None;
    }
    Some(btime + ticks / ticks_per_sec)
}

/// Scan-run summary from `scan.metadata`. None when the run never
/// completed or is an agent run.
fn scan_summary(run: &scan::Scan) -> anyhow::Result<Option<scan::ScanSummary>> {
    Ok(match run.parse_metadata()? {
        Some(scan::ScanMetadata::Scan(s)) => Some(s),
        Some(scan::ScanMetadata::Agent(_) | scan::ScanMetadata::Running(_)) | None => None,
    })
}

async fn view(args: ScanViewArgs) {
    // No scan arg: the view opens with its scan picker dialog.
    let model = match args.scan_id {
        Some(id) => {
            let conn = db::open_db().unwrap();
            match load_scan_model(&conn, &id) {
                Ok(m) => Some(m),
                Err(e) => {
                    eprintln!("gage scan view: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => None,
    };
    let load = move |id: &str| -> std::io::Result<gage_tui::scan_view::ScanModel> {
        let conn = db::open_db().map_err(std::io::Error::other)?;
        load_scan_model(&conn, id).map_err(std::io::Error::other)
    };
    if let Err(e) = gage_tui::scan_view::view(model, load).await {
        eprintln!("gage scan view: {e}");
        std::process::exit(1);
    }
}

/// Assemble a [`ScanModel`] for a completed scan from the db: tasks
/// from `scan_task`, the run summary from `scan.metadata`, and
/// sessions/notes/issues from the scan's edge tables. A scan that
/// never completed has no summary metadata; the header shows no
/// progress and non-terminal tasks render in their last known state.
fn load_scan_model(
    conn: &gage_db::rusqlite::Connection,
    prefix: &str,
) -> anyhow::Result<gage_tui::scan_view::ScanModel> {
    use gage_tui::scan_view::{ScanModel, SessionItem, TaskId, TaskItem, TaskState};
    use std::collections::HashMap;

    let run = scan::get_scan(conn, prefix)?;
    let summary = scan_summary(&run)?;

    let mut agent_times = AgentTimes::default();
    let mut session_displays = SessionDisplays::default();
    let results = load_scan_results(conn, &run.id, &mut agent_times, &mut session_displays)?;

    let mut tasks: Vec<TaskItem> = scan::tasks_for_scan(conn, &run.id)?
        .into_iter()
        .map(|t| TaskItem {
            cost: results
                .task_costs
                .iter()
                .find(|tc| tc.id.scanner == t.scanner_name && tc.id.task == t.task_name)
                .map(|tc| tc.cost),
            id: TaskId {
                scanner: t.scanner_name,
                task: t.task_name,
            },
            state: match t.status {
                scan::TaskStatus::Pending => TaskState::Pending,
                scan::TaskStatus::Started => TaskState::Running,
                scan::TaskStatus::Completed => TaskState::Completed,
                scan::TaskStatus::Failed => TaskState::Error,
                scan::TaskStatus::Skipped => TaskState::Skipped,
                scan::TaskStatus::Canceled => TaskState::Canceled,
            },
            elapsed: match (t.started, t.stopped) {
                (Some(a), Some(b)) => Some(Duration::from_millis(b.saturating_sub(a) as u64)),
                _ => None,
            },
            started: None,
            progress: None,
            agents: Vec::new(),
        })
        .collect();
    for ta in &results.agents {
        if let Some(item) = tasks.iter_mut().find(|t| t.id == ta.task) {
            item.agents.push(ta.agent.clone());
        }
    }
    let errors = tasks.iter().filter(|t| t.state == TaskState::Error).count();

    let counts: HashMap<&str, (usize, usize)> = results
        .sessions
        .iter()
        .map(|c| (c.id.as_str(), (c.notes, c.issues)))
        .collect();

    // Per-corpus lookup: `scan_session.metadata` says where each
    // session lives, independent of the process's `-A` mode
    let rows = scan::scan_session_rows(conn, &run.id)?;
    let store = gage_query::default_index_store();
    let paths: HashMap<String, std::path::PathBuf> = session::ls_sessions().into_iter().collect();
    let (agent_store, agent_paths) = if rows.iter().any(|r| r.agent) {
        (Some(gage_query::agent_index_store()), agent_session_paths())
    } else {
        (None, HashMap::new())
    };
    let mut sessions: Vec<SessionItem> = rows
        .into_iter()
        .map(|row| {
            let (store, paths) = if row.agent {
                (agent_store.as_ref().unwrap(), &agent_paths)
            } else {
                (&store, &paths)
            };
            let id = row.session_id;
            let title = stat_session(paths, &id)
                .map(|info| session_title(store, &info))
                .unwrap_or_else(|| "(unavailable)".to_string());
            let (notes, issues) = counts.get(id.as_str()).copied().unwrap_or((0, 0));
            SessionItem {
                notes,
                issues,
                path: paths.get(&id).cloned(),
                id,
                title,
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
        scan_id: short_uuid(&run.id).to_string(),
        out_path: Some(ScanStreams::out_path(&run.id)),
        total: summary.as_ref().map(|s| s.total).unwrap_or(0),
        progress: summary
            .as_ref()
            .map(|s| s.completed + s.failed + s.skipped)
            .unwrap_or(0),
        notes: results.notes,
        issues: results.issues,
        cost: results.cost,
        errors,
        finished: true,
        elapsed: run
            .parse_metadata()?
            .and_then(|m| m.elapsed_ms())
            .map(Duration::from_millis),
        tasks,
        sessions,
    })
}

/// One reconcile pass for the live view: load the scan's results from
/// the db as a Results event, or a Log event when the read fails.
fn reconcile_results(
    db: &Arc<Mutex<gage_db::rusqlite::Connection>>,
    scan_id: &str,
    agent_times: &Mutex<AgentTimes>,
    session_displays: &Mutex<SessionDisplays>,
) -> gage_tui::scan_view::Event {
    let results = {
        let conn = db.lock().unwrap();
        let mut times = agent_times.lock().unwrap();
        let mut displays = session_displays.lock().unwrap();
        load_scan_results(&conn, scan_id, &mut times, &mut displays)
    };
    match results {
        Ok(r) => gage_tui::scan_view::Event::Results {
            notes: r.notes,
            issues: r.issues,
            sessions: r.sessions,
            cost: r.cost,
            task_costs: r.task_costs,
            agents: r.agents,
        },
        Err(e) => gage_tui::scan_view::Event::Log(format!("results refresh failed: {e}")),
    }
}

/// Notes, issues, per-session counts, and agent sessions recorded for
/// a scan — shared between the historical loader and the live view's
/// reconcile poll.
struct ScanResults {
    notes: Vec<gage_tui::scan_view::NoteItem>,
    issues: Vec<gage_tui::scan_view::IssueItem>,
    sessions: Vec<gage_tui::scan_view::SessionCounts>,
    cost: Option<gage_tui::scan_view::ScanCost>,
    task_costs: Vec<gage_tui::scan_view::TaskCost>,
    agents: Vec<gage_tui::scan_view::TaskAgent>,
}

fn load_scan_results(
    conn: &gage_db::rusqlite::Connection,
    scan_id: &str,
    agent_times: &mut AgentTimes,
    session_displays: &mut SessionDisplays,
) -> anyhow::Result<ScanResults> {
    use gage_tui::scan_view::{EventItem, EvidenceItem, IssueItem, NoteItem, SessionCounts};
    use std::collections::{HashMap, HashSet};

    let notes = gage_db::note::find(
        conn,
        &gage_db::note::NoteFilters {
            scan: Some(scan_id.to_string()),
            ..Default::default()
        },
    )?;
    let issue_ids: HashSet<String> = scan::issue_ids_for_scan(conn, scan_id)?
        .into_iter()
        .collect();
    let issues: Vec<gage_db::issue::Issue> = gage_db::issue::find(
        conn,
        &gage_db::issue::IssueFilters {
            status: gage_db::issue::IssueStatusFilter::Any,
            ..Default::default()
        },
    )?
    .into_iter()
    .filter(|i| issue_ids.contains(&i.id))
    .collect();

    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    for note in &notes {
        if let gage_db::target::NoteTarget::Session(t) = &note.target {
            counts.entry(t.session_id.clone()).or_default().0 += 1;
        }
    }
    // An issue attributes to every session it is linked to. The links
    // are recorded at write time from the writer's explicit sessions
    // and the sessions its evidence notes target.
    for issue in &issues {
        for session_id in gage_db::issue::issue_sessions(conn, &issue.id)? {
            counts.entry(session_id).or_default().1 += 1;
        }
    }

    let mut issue_items: Vec<IssueItem> = Vec::with_capacity(issues.len());
    for i in &issues {
        let evidence = gage_db::issue::related_notes(conn, &i.id)?
            .iter()
            .map(|n| EvidenceItem {
                id: n.id.clone(),
                name: n.name.clone(),
                target: n.target.to_uri(),
                value: crate::cmd_note::format_value(&n.value),
            })
            .collect();
        let events = gage_db::issue::issue_events_for(conn, &i.id)?
            .iter()
            .map(|ev| EventItem {
                kind: ev.event.to_label(),
                author: ev.author.clone(),
                timestamp: gage_core::datetime::ms_to_iso8601(ev.timestamp),
                message: ev.event.message().map(str::to_string),
            })
            .collect();
        issue_items.push(IssueItem {
            id: i.id.clone(),
            name: i.name.clone(),
            title: i.title.lines().next().unwrap_or("").to_string(),
            status: match i.status_reason {
                Some(r) => format!("{} ({})", i.status.as_str(), r.as_str()),
                None => i.status.as_str().to_string(),
            },
            status_cell: match i.status_reason {
                Some(r) => r.as_str().to_string(),
                None => i.status.as_str().to_string(),
            },
            closed: i.status == gage_db::issue::IssueStatus::Closed,
            author: i.author.clone(),
            created: gage_core::datetime::ms_to_iso8601(i.created),
            description: i.description.clone(),
            sessions: issue_session_items(conn, &i.id, session_displays)?,
            evidence,
            events,
        });
    }

    let cost = scan::cost_for_scan(conn, scan_id)?;
    let cost =
        (cost.total_usd != 0.0 || cost.incomplete).then_some(gage_tui::scan_view::ScanCost {
            usd: cost.total_usd,
            incomplete: cost.incomplete,
        });
    let task_costs = scan::costs_for_tasks(conn, scan_id)?
        .into_iter()
        .map(|tc| gage_tui::scan_view::TaskCost {
            id: gage_tui::scan_view::TaskId {
                scanner: tc.scanner_name,
                task: tc.task_name,
            },
            cost: gage_tui::scan_view::ScanCost {
                usd: tc.cost.total_usd,
                incomplete: tc.cost.incomplete,
            },
        })
        .collect();

    let agents = scan::agents_for_scan(conn, scan_id)?
        .iter()
        .map(|row| agent_item(row, agent_times))
        .collect();

    Ok(ScanResults {
        cost,
        task_costs,
        agents,
        issues: issue_items,
        notes: notes
            .iter()
            .map(|n| NoteItem {
                id: n.id.clone(),
                name: n.name.clone(),
                value: crate::cmd_note::format_value_cell(&n.value),
                value_full: crate::cmd_note::format_value(&n.value),
                target: n.target.to_uri(),
                target_cell: crate::cmd_note::target_label(&n.target),
                author: n.author.clone(),
                created: gage_core::datetime::ms_to_iso8601(n.created),
                metadata: n.metadata.clone(),
            })
            .collect(),
        sessions: counts
            .into_iter()
            .map(|(id, (notes, issues))| SessionCounts { id, notes, issues })
            .collect(),
    })
}

/// Resolve an issue's linked sessions for display: project path and
/// session title per link, from the cache.
fn issue_session_items(
    conn: &gage_db::rusqlite::Connection,
    issue_id: &str,
    displays: &mut SessionDisplays,
) -> anyhow::Result<Vec<gage_tui::scan_view::IssueSessionItem>> {
    let mut items = Vec::new();
    for id in gage_db::issue::issue_sessions(conn, issue_id)? {
        let display = displays.resolve(&id);
        items.push(gage_tui::scan_view::IssueSessionItem {
            id,
            project: display.project,
            title: display.title,
        });
    }
    Ok(items)
}

/// Session display fields per linked session, cached across reconcile
/// ticks like [`AgentTimes`] — titles and project paths do not change
/// while a view is open, so each session resolves once.
#[derive(Default)]
struct SessionDisplays {
    map: std::collections::HashMap<String, SessionDisplay>,
    /// Session walk from the last unknown id; sessions can appear
    /// while a scan runs, so an id missing from it refreshes the walk
    paths: std::collections::HashMap<String, std::path::PathBuf>,
    /// Resolved project display per encoded directory name
    projects: std::collections::HashMap<String, String>,
}

#[derive(Clone)]
struct SessionDisplay {
    project: String,
    title: String,
}

impl SessionDisplays {
    /// A session's display fields. Both fall back to a placeholder
    /// when the session is not on disk.
    fn resolve(&mut self, id: &str) -> SessionDisplay {
        if let Some(display) = self.map.get(id) {
            return display.clone();
        }
        if !self.paths.contains_key(id) {
            self.paths = session::ls_sessions().into_iter().collect();
        }
        let display = match stat_session(&self.paths, id) {
            Some(info) => {
                let encoded = info.project_name().into_owned();
                let home = ClaudeHome::from_env().ok();
                let project = self
                    .projects
                    .entry(encoded)
                    .or_insert_with_key(|encoded| project_display(home.as_ref(), encoded))
                    .clone();
                let store = gage_query::default_index_store();
                SessionDisplay {
                    project,
                    title: session_title(&store, &info),
                }
            }
            None => SessionDisplay {
                project: "(unavailable)".to_string(),
                title: "(unavailable)".to_string(),
            },
        };
        self.map.insert(id.to_string(), display.clone());
        display
    }
}

/// Adapt a `task_agent` row to the scan view's agent entry: state and
/// cost from the recorded terminal result, session path from the gage
/// archive, and time bounds from the session JSONL via the cache.
fn agent_item(row: &scan::TaskAgent, times: &mut AgentTimes) -> gage_tui::scan_view::TaskAgent {
    use gage_tui::scan_view::{AgentItem, AgentState, TaskId};

    let result = row.result.as_deref().and_then(|raw| {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(session_id = %row.session_id, "unparseable agent result: {e}");
                None
            }
        }
    });
    let state = match (&result, row.exit_code) {
        (Some(r), _) => {
            let is_error = r
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if is_error {
                AgentState::Error
            } else {
                AgentState::Done
            }
        }
        (None, Some(_)) => AgentState::Error,
        (None, None) => AgentState::Running,
    };
    let cost = result
        .as_ref()
        .and_then(|r| r.get("total_cost_usd"))
        .and_then(serde_json::Value::as_f64);
    let path = gage_claude::session::find_agent_session(&row.session_id);
    let terminal = state != AgentState::Running;
    let (started_ms, ended_ms) = times.resolve(&row.session_id, path.as_deref(), terminal);
    gage_tui::scan_view::TaskAgent {
        task: TaskId {
            scanner: row.scanner_name.clone(),
            task: row.task_name.clone(),
        },
        agent: AgentItem {
            session_id: row.session_id.clone(),
            path,
            state,
            cost,
            started_ms,
            ended_ms,
        },
    }
}

/// Session time bounds per agent, cached across reconcile ticks so a
/// tick does not re-walk every agent JSONL. The start is read once at
/// discovery; the end is read once more after the agent's row records
/// a terminal outcome, and is only reported once final — a running
/// agent's duration is measured against now, not its last write.
#[derive(Default)]
struct AgentTimes {
    map: std::collections::HashMap<String, AgentTimesEntry>,
}

struct AgentTimesEntry {
    started_ms: Option<i64>,
    ended_ms: Option<i64>,
    /// The bounds were read after the terminal outcome was recorded,
    /// so `ended_ms` is final.
    terminal_read: bool,
}

impl AgentTimes {
    fn resolve(
        &mut self,
        session_id: &str,
        path: Option<&std::path::Path>,
        terminal: bool,
    ) -> (Option<i64>, Option<i64>) {
        if let Some(e) = self.map.get(session_id)
            && e.started_ms.is_some()
            && (e.terminal_read || !terminal)
        {
            return (e.started_ms, e.ended_ms.filter(|_| e.terminal_read));
        }
        let Some(path) = path else {
            return (None, None);
        };
        match gage_claude::stats::compute_session_stats(path) {
            Ok(stats) => {
                self.map.insert(
                    session_id.to_string(),
                    AgentTimesEntry {
                        started_ms: stats.started_ms,
                        ended_ms: stats.ended_ms,
                        terminal_read: terminal,
                    },
                );
                (stats.started_ms, stats.ended_ms.filter(|_| terminal))
            }
            Err(e) => {
                tracing::warn!(session_id, "agent session stats unavailable: {e}");
                (None, None)
            }
        }
    }
}

/// Session (id, path) pairs from the agent corpus, for scans whose
/// `scan_session` rows are marked `corpus=agent`.
fn agent_session_paths() -> std::collections::HashMap<String, std::path::PathBuf> {
    SessionListBuilder::new()
        .root(gage_core::config::agent_sessions_dir())
        .build()
        .into_iter()
        .map(|s| (s.id, s.src))
        .collect()
}

/// Locate and stat a session file for title resolution. None when the
/// session no longer exists on disk — expected for old scans.
fn stat_session(
    paths: &std::collections::HashMap<String, std::path::PathBuf>,
    id: &str,
) -> Option<SessionInfo> {
    let src = paths.get(id)?;
    let meta = std::fs::metadata(src).ok()?;
    Some(SessionInfo {
        id: id.to_string(),
        src: src.clone(),
        mtime: meta.modified().unwrap(),
        size: meta.len(),
    })
}

fn delete(args: ScanDeleteArgs) {
    if args.ids.is_empty() {
        eprintln!(
            "gage scan delete: provide one or more scan run IDs\n\n\
             Use 'gage scan list' to show scan runs"
        );
        std::process::exit(1);
    }

    let conn = db::open_db().unwrap();

    let mut runs: Vec<scan::Scan> = Vec::new();
    let mut errors = 0;
    for prefix in &args.ids {
        match scan::get_scan(&conn, prefix) {
            Ok(r) => runs.push(r),
            Err(e) => {
                eprintln!("{e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let run_count = runs.len();

    dialog::run("Delete scan runs", || {
        let run_plural = if run_count == 1 { "run" } else { "runs" };
        cli::log::remark(format!("{run_count} {run_plural}"))?;

        if !args.yes {
            let prompt =
                format!("Permanently delete {run_count} scan {run_plural}? This cannot be undone.");
            let confirmed = cli::confirm(prompt).initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let mut deleted = 0;
        for run in &runs {
            if let Err(e) = scan::delete_scan(&conn, &run.id) {
                eprintln!("warning: failed to delete {}: {e}", short_uuid(&run.id));
            } else {
                deleted += 1;
                remove_scan_logs(&run.id);
            }
        }

        let plural = if deleted == 1 {
            "scan run"
        } else {
            "scan runs"
        };
        Ok(DialogResult::from(format!("Deleted {deleted} {plural}")))
    });
}

fn invalidate(args: ScanInvalidateArgs) {
    let conn = db::open_db().unwrap();
    dialog::run("Invalidate scan tasks", || {
        let selectors = [
            ("Sessions", like_filter("ref", "session:", &args.sessions)),
            ("Notes", like_filter("ref", "note:", &args.notes)),
            ("Tasks", like_filter("key", "", &args.tasks)),
        ];

        let mut counts = String::new();
        let mut clauses: Vec<String> = Vec::new();
        let mut patterns: Vec<String> = Vec::new();
        for (label, selector) in selectors {
            let Some((clause, pats)) = selector else {
                continue;
            };
            let n = count_matching_tasks(&conn, &clause, &pats)?;
            counts.push_str(&format!("\n{}", style(format!("{label}: {n}")).dim()));
            clauses.push(clause);
            patterns.extend(pats);
        }
        cli::log::remark(format!("Matching tasks{counts}"))?;

        let clause = clauses.join(" OR ");
        let count = count_matching_tasks(&conn, &clause, &patterns)?;
        if count == 0 {
            return Ok(DialogResult::from("No matching tasks"));
        }

        if !args.yes {
            let prompt = format!("You are about to invalidate {count} tasks. Continue?");
            let confirmed = cli::confirm(prompt).initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let n = conn
            .execute(
                &format!("DELETE FROM task_validate WHERE {clause}"),
                gage_db::rusqlite::params_from_iter(&patterns),
            )
            .context("deleting task validation rows")?;
        Ok(DialogResult::from(format!("{n} tasks invalidated")))
    });
}

/// LIKE clause and patterns prefix-matching `column` against each id,
/// or None when no ids were given for this selector.
fn like_filter(column: &str, prefix: &str, ids: &[String]) -> Option<(String, Vec<String>)> {
    if ids.is_empty() {
        return None;
    }
    let clause: Vec<String> = ids.iter().map(|_| format!("{column} LIKE ?")).collect();
    let patterns = ids.iter().map(|id| format!("{prefix}{id}%")).collect();
    Some((format!("({})", clause.join(" OR ")), patterns))
}

fn count_matching_tasks(
    conn: &gage_db::rusqlite::Connection,
    clause: &str,
    patterns: &[String],
) -> Result<usize, DialogError> {
    let count: usize = conn
        .query_row(
            &format!("SELECT count(*) FROM task_validate WHERE {clause}"),
            gage_db::rusqlite::params_from_iter(patterns),
            |row| row.get(0),
        )
        .context("counting task validation rows")?;
    Ok(count)
}

async fn run_scan(mut args: ScanRunArgs) {
    let mut registry = ScannerRegistry::load();

    if args.list_scanners {
        list_scanners(&registry);
        return;
    }

    // --rerun expands into the explicit scanner and session lists, then
    // flows through the normal run path below.
    if let Some(prefix) = &args.rerun {
        let conn = db::open_db().unwrap();
        match rerun_args(&conn, prefix) {
            Ok((scanners, sessions)) => {
                args.scanners = scanners;
                args.sessions = sessions;
            }
            Err(e) => {
                eprintln!("gage scan: {e}");
                std::process::exit(1);
            }
        }
    }

    // --scan expands into the target scan's agent sessions ("scanning
    // a scan") and implies the agent corpus; scanner selection narrows
    // to the `eval` group in run_dialog.
    if let Some(prefix) = &args.scan {
        crate::set_agent_projects_dir();
        let conn = db::open_db().unwrap();
        match scan_scan_args(&conn, prefix, args.limit, args.sample) {
            Ok(sessions) => args.sessions = sessions,
            Err(e) => {
                eprintln!("gage scan: {e}");
                std::process::exit(1);
            }
        }
    }

    // Register `-f` files into the registry and append their composite
    // names to the explicit scanner list. Any `#{...}` config override
    // suffix on the path is split off first and re-appended to the
    // composite name so `Scanner::from_spec` parses it normally.
    let mut file_specs: Vec<String> = Vec::new();
    let mut errors = 0;
    for raw in &args.files {
        let (path_str, override_suffix) = match raw.find("#{") {
            Some(pos) => (&raw[..pos], &raw[pos..]),
            None => (raw.as_str(), ""),
        };
        match registry.register_file(std::path::Path::new(path_str)) {
            Ok(name) => {
                // The same file given twice (paths canonicalize to one
                // composite name) runs once per distinct config override.
                let spec = format!("{name}{override_suffix}");
                if !file_specs.contains(&spec) {
                    file_specs.push(spec);
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }
    args.scanners.extend(file_specs);

    let explicit_sessions: Option<Vec<(String, std::path::PathBuf)>> = if args.sessions.is_empty() {
        None
    } else {
        let mut resolved = Vec::new();
        let mut errors = 0;
        for prefix in &args.sessions {
            match session::one_session(prefix) {
                Ok(s) => resolved.push((s.id, s.src)),
                Err(e) => {
                    eprintln!("{e}");
                    errors += 1;
                }
            }
        }
        if errors > 0 {
            std::process::exit(1);
        }
        Some(resolved)
    };

    // The scan id is minted here — before the runner — so the log
    // files and the db rows share one key from the start.
    let scan_id = gage_core::uuid::new_uuid();
    let _log_guard = match gage_log::init_named("scan", &scan_id, "warn,gage_runtime=info") {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error initializing scan log: {e}");
            std::process::exit(1);
        }
    };

    dialog::run_async("Scan sessions", || {
        run_dialog(args, registry, explicit_sessions, scan_id)
    })
    .await;
}

/// Resolve a prior scan into the scanner names and session IDs to run
/// again. Session paths are re-resolved downstream, so sessions that no
/// longer exist on disk fail there with the standard message.
fn rerun_args(
    conn: &gage_db::rusqlite::Connection,
    prefix: &str,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let run = scan::get_scan(conn, prefix)?;
    let scanners = scan::scanner_names_for_scan(conn, &run.id)?;
    if scanners.is_empty() {
        anyhow::bail!("scan {} has no scanners", short_uuid(&run.id));
    }
    let sessions = scan::session_ids_for_scan(conn, &run.id)?;
    if sessions.is_empty() {
        anyhow::bail!("scan {} has no sessions", short_uuid(&run.id));
    }
    Ok((scanners, sessions))
}

/// Resolve `--scan` into the target scan's agent session ids, applying
/// `--sample` (random N) then `--limit` (first N) to the expanded
/// list.
fn scan_scan_args(
    conn: &gage_db::rusqlite::Connection,
    prefix: &str,
    limit: Option<usize>,
    sample: Option<usize>,
) -> anyhow::Result<Vec<String>> {
    let run = scan::get_scan(conn, prefix)?;
    let mut sessions = scan::agent_session_ids_for_scan(conn, &run.id)?;
    if sessions.is_empty() {
        anyhow::bail!("scan {} has no agent sessions", short_uuid(&run.id));
    }
    if let Some(n) = sample {
        sessions.shuffle(&mut rand::rng());
        sessions.truncate(n);
        sessions.sort();
    }
    if let Some(n) = limit {
        sessions.truncate(n);
    }
    Ok(sessions)
}

/// Capture files for the scan's output streams:
/// `~/.gage/log/scan/{scan_id}.out` (scanner stdout) and `.err`
/// (warnings and task failures). Creation failure disables capture
/// for the run; the scan itself proceeds.
struct ScanStreams {
    out: Option<std::fs::File>,
    err: Option<std::fs::File>,
}

impl ScanStreams {
    fn with_scan(scan_id: &str) -> Self {
        let dir = gage_log::role_dir("scan");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("creating scan log dir {}: {e}", dir.display());
            return Self {
                out: None,
                err: None,
            };
        }
        let open = |ext: &str| {
            let path = dir.join(format!("{scan_id}.{ext}"));
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!("opening {}: {e}", path.display());
                    None
                }
            }
        };
        Self {
            out: open("out"),
            err: open("err"),
        }
    }

    fn out(&mut self, s: &str) {
        write_stream(&mut self.out, s);
    }

    fn out_line(&mut self, s: &str) {
        write_stream(&mut self.out, s);
        write_stream(&mut self.out, "\n");
    }

    fn err_line(&mut self, s: &str) {
        write_stream(&mut self.err, s);
        write_stream(&mut self.err, "\n");
    }

    /// Path of the `.out` capture, for the scan view's log dialog.
    fn out_path(scan_id: &str) -> std::path::PathBuf {
        gage_log::role_dir("scan").join(format!("{scan_id}.out"))
    }
}

fn write_stream(file: &mut Option<std::fs::File>, s: &str) {
    use std::io::Write;
    if let Some(f) = file
        && let Err(e) = f.write_all(s.as_bytes())
    {
        // Disable capture after a write failure rather than logging
        // once per line
        tracing::warn!("scan stream write failed, disabling capture: {e}");
        *file = None;
    }
}

async fn run_dialog(
    args: ScanRunArgs,
    registry: ScannerRegistry,
    explicit_sessions: Option<Vec<(String, std::path::PathBuf)>>,
    scan_id: String,
) -> Result<DialogResult, DialogError> {
    // Scanner selection — default set excludes disabled scanners.
    // Explicit `-s name` (handled below) still runs disabled scanners.
    let cwd = std::env::current_dir().context("reading current working directory")?;
    let (config, _) = gage_core::config::load_merged(&cwd)
        .with_context(|| format!("loading merged config from {}", cwd.display()))?;
    // "Scanning a scan" narrows the pickable set to the `eval` group
    let eval_mode = args.scan.is_some();
    let defs = registry.list_enabled(&config);
    let mut names: Vec<&str> = if eval_mode {
        registry
            .group_members("eval")
            .into_iter()
            .filter(|d| config.is_scanner_enabled(&d.name))
            .map(|d| d.name.as_str())
            .collect()
    } else {
        defs.iter().map(|d| d.name.as_str()).collect()
    };
    names.sort();
    if eval_mode && names.is_empty() {
        cli::log::error("No scanners for group 'eval'")?;
        return Err(DialogError::Canceled);
    }

    // `-g` expands to the group's enabled members, unioned with `-s`
    let mut group_names: Vec<String> = Vec::new();
    for group in &args.groups {
        let members: Vec<&str> = registry
            .group_members(group)
            .into_iter()
            .filter(|d| config.is_scanner_enabled(&d.name))
            .map(|d| d.name.as_str())
            .collect();
        if members.is_empty() {
            cli::log::error(format!("No scanners for group '{group}'"))?;
            return Err(DialogError::Canceled);
        }
        for name in members {
            if !group_names.iter().any(|n| n == name) {
                group_names.push(name.to_string());
            }
        }
    }

    let selected_names: Vec<String> =
        if args.scanners.is_empty() && group_names.is_empty() && !args.yes {
            // No selection args: pick interactively — `default` group
            // members pre-selected, or the whole (eval-only) list
            // under `--scan`
            let default_names: Vec<usize> = if eval_mode {
                (0..names.len()).collect()
            } else {
                names
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| {
                        registry
                            .group_members("default")
                            .iter()
                            .any(|d| d.name == **n)
                    })
                    .map(|(i, _)| i)
                    .collect()
            };
            let mut prompt = cli::multiselect("Scanners").initial_values(default_names);
            for (i, name) in names.iter().enumerate() {
                prompt = prompt.item(i, (*name).to_string(), "");
            }
            let indices: Vec<usize> = prompt.interact()?;
            indices
                .iter()
                .map(|&i| {
                    names
                        .get(i)
                        .expect("selected holds positions in names")
                        .to_string()
                })
                .collect()
        } else if args.scanners.is_empty() && group_names.is_empty() {
            // `-y` with no selection args: the `default` group, or the
            // whole (eval-only) list under `--scan`
            if eval_mode {
                names.iter().map(|n| n.to_string()).collect()
            } else {
                names
                    .iter()
                    .filter(|n| {
                        registry
                            .group_members("default")
                            .iter()
                            .any(|d| d.name == **n)
                    })
                    .map(|n| n.to_string())
                    .collect()
            }
        } else {
            for name in &args.scanners {
                let bare = name.split("#{").next().unwrap();
                // Library scanners are not selectable — same error as an
                // unknown name
                if !registry.is_known(bare) || registry.is_library(bare) {
                    cli::log::error(format!("Unknown scanner: {bare}"))?;
                    return Err(DialogError::Canceled);
                }
            }
            let mut out = group_names;
            for name in &args.scanners {
                if !out.iter().any(|n| n == name) {
                    out.push(name.clone());
                }
            }
            out
        };

    if !args.scanners.is_empty() || !args.groups.is_empty() || eval_mode || args.yes {
        let display: Vec<&str> = selected_names
            .iter()
            .map(|n| n.split("#{").next().unwrap())
            .collect();
        let scanner_lines: String = display
            .iter()
            .map(|n| format!("\n{}", style(n).dim()))
            .collect();
        cli::log::step(format!("Scanners{scanner_lines}"))?;
    }

    let mut scanners: Vec<Scanner<'_>> = {
        let mut out = Vec::new();
        for spec in &selected_names {
            match Scanner::from_spec(spec, &registry) {
                Ok(s) => out.push(s),
                Err(e) => {
                    cli::log::error(format!("{e}"))?;
                    return Err(DialogError::Canceled);
                }
            }
        }
        out
    };

    // Pull in tasks that declare themselves `required_by` what the
    // selection writes
    let required = {
        let selected_defs: Vec<_> = scanners.iter().map(|s| s.def).collect();
        registry.required_tasks(&selected_defs, &config)
    };
    for (def, tasks) in required {
        scanners.push(Scanner::with_tasks(def, tasks));
    }

    // Compile all scanners up front. A scanner that does not compile
    // (or declares a task with no matching function) is a full stop:
    // the error is shown here and the scan never starts — no scan
    // record, no UI.
    let db = Arc::new(Mutex::new(db::open_db().unwrap()));
    let slots = match gage_scan::runner::compile_scanners(&scanners, db.clone()) {
        Ok(slots) => slots,
        Err(e) => {
            cli::log::error(format!("{e}"))?;
            return Err(DialogError::Failed(
                "Scan not started due to scanner errors".to_string(),
            ));
        }
    };

    // Session selection
    let sessions = if let Some(resolved) = explicit_sessions {
        let mut session_lines: String = resolved
            .iter()
            .take(5)
            .map(|(id, _)| format!("\n{}", style(id).dim()))
            .collect();
        let more = resolved.len().saturating_sub(5);
        if more > 0 {
            let label = format!("{more} more session{}", if more == 1 { "" } else { "s" });
            session_lines.push_str(&format!("\n{}", style(label).dim().italic()));
        }
        cli::log::step(format!("Sessions{session_lines}"))?;
        resolved
    } else {
        // Any selection option means the user chose the selection:
        // only what they specified applies. With no selection options
        // the default is the last 30 days, up to 50 sessions.
        let no_selection = !args.all
            && !args.today
            && args.days.is_none()
            && args.limit.is_none()
            && args.sample.is_none();
        let since = if args.all {
            None
        } else if args.today {
            Some(since_local_midnight())
        } else if let Some(d) = args.days {
            Some(Duration::from_secs(u64::from(d) * 86_400))
        } else if args.sample.is_some() || no_selection {
            // --sample's documented default window; also the bare-
            // invocation default
            Some(Duration::from_secs(30 * 86_400))
        } else {
            // --limit alone: latest N across all time
            None
        };
        let limit = if no_selection { Some(50) } else { args.limit };

        let project = match &args.project {
            Some(p) => {
                let projects = known_projects()
                    .await
                    .context("querying session projects")?;
                let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
                match resolve_project(p, &home, &cwd, &projects) {
                    Some(slug) => Some(slug),
                    None => {
                        cli::log::error(format!("No sessions for project '{p}'"))?;
                        return Err(DialogError::Failed("Nothing to scan".to_string()));
                    }
                }
            }
            None => None,
        };

        let mut label = if args.all {
            "all".to_string()
        } else if since.is_none() {
            let n = args
                .limit
                .expect("limit is the only capless windowless selection");
            format!("{n} latest")
        } else {
            let window = if args.today {
                "today".to_string()
            } else {
                let d = args.days.unwrap_or(30);
                format!("last {d} day{}", if d == 1 { "" } else { "s" })
            };
            match (args.sample, limit) {
                (Some(n), _) => format!("{n} sampled from {window}"),
                (None, Some(n)) => format!("{window}, max {n}"),
                (None, None) => window,
            }
        };
        if let Some(slug) = &project {
            label = format!("{label} in {}", project_slug_display(slug));
        }
        cli::log::step(format!("Sessions\n{}", style(label).dim()))?;

        let mut builder = SessionListBuilder::new();
        if let Some(slug) = &project {
            builder = builder.project_slug(slug.clone());
        }
        if let Some(d) = since {
            builder = builder.since(d);
        }
        if let Some(n) = limit {
            builder = builder.limit(n);
        }
        let mut sessions: Vec<(String, std::path::PathBuf)> =
            builder.build().into_iter().map(|s| (s.id, s.src)).collect();
        if let Some(n) = args.sample {
            sessions.shuffle(&mut rand::rng());
            sessions.truncate(n);
        }
        sessions
    };

    // Confirmation
    if !args.yes {
        let confirmed = cli::confirm("Run this scan?")
            .initial_value(true)
            .interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    // Run
    let started = std::time::Instant::now();
    let jobs = args.jobs.unwrap_or_else(num_cpus::get).max(1);
    let agent_jobs = args.agent_jobs.unwrap_or(DEFAULT_AGENT_JOBS).max(1);

    // Enrich (id, path) → SessionInfo for the DataFusion-side context
    // built next.
    let selected: Arc<[SessionInfo]> = {
        let mut out: Vec<SessionInfo> = Vec::with_capacity(sessions.len());
        for (id, src) in sessions {
            let meta = std::fs::metadata(&src)
                .with_context(|| format!("stat session file {}", src.display()))?;
            let mtime = meta.modified().unwrap();
            out.push(SessionInfo {
                id,
                src,
                mtime,
                size: meta.len(),
            });
        }
        Arc::from(out.into_boxed_slice())
    };
    let scan_ctx = Arc::new(ScanSessionContext::new(&selected));

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

    // Notes and issues are no longer tied to a scan, so "what this scan
    // produced" is derived by diffing before/after: a note count for the
    // summary, and the set of issue ids so the new issues can be listed.
    // This assumes Gage controls DB access and no concurrent scan runs —
    // anything added out-of-band during the scan would be miscounted.
    let (notes_before, issue_ids_before) = {
        let conn = db.lock().unwrap();
        (
            gage_db::note::count(&conn).unwrap_or(0),
            all_issue_ids(&conn),
        )
    };

    let mut streams = ScanStreams::with_scan(&scan_id);
    let result = if args.no_progress {
        // Headless: no TUI, scanner output and diagnostics stream to
        // the terminal
        gage_scan::runner::run(
            db.clone(),
            scan_id,
            scanners,
            slots,
            selected,
            scan_ctx,
            jobs,
            agent_jobs,
            cancel.clone(),
            |event| {
                capture_event(&mut streams, &event);
                use std::io::Write;
                match &event {
                    gage_scan::event::ScanEvent::Print { s } => {
                        std::io::stdout()
                            .write_all(s.as_bytes())
                            .expect("write to stdout");
                    }
                    gage_scan::event::ScanEvent::Println { s } => {
                        println!("{s}");
                    }
                    gage_scan::event::ScanEvent::TaskFailed {
                        scanner,
                        task,
                        message,
                    } => {
                        eprintln!("error: {}", task_display(scanner, task));
                        for line in message.lines() {
                            eprintln!("{line}");
                        }
                    }
                    gage_scan::event::ScanEvent::Warning {
                        scanner,
                        task,
                        message,
                    } => {
                        eprintln!("warning: {}: {message}", task_display(scanner, task));
                    }
                    gage_scan::event::ScanEvent::Status(_) => {}
                }
            },
        )
        .await
    } else {
        run_scan_tui(
            db.clone(),
            scan_id,
            scanners,
            slots,
            selected,
            scan_ctx,
            jobs,
            agent_jobs,
            cancel.clone(),
            streams,
        )
        .await
    };
    let elapsed = started.elapsed();

    cancel.cancel();
    if let Err(e) = signal_task.await
        && !e.is_cancelled()
    {
        panic!("signal task joined cleanly: {e}");
    }

    match result {
        Ok(summary) => {
            let skipped_suffix = if summary.skipped > 0 {
                format!(", {} skipped", summary.skipped)
            } else {
                String::new()
            };
            let new_notes = {
                let conn = db.lock().unwrap();
                let new_issues = new_issues_since(&conn, &issue_ids_before);
                render_issues_remark(&new_issues)?;
                gage_db::note::count(&conn)
                    .unwrap_or(notes_before)
                    .saturating_sub(notes_before)
            };
            Ok(DialogResult::from(format!(
                "{} tasks in {}{skipped_suffix}, {new_notes} new notes (scan {})",
                summary.completed,
                crate::human::format_duration(elapsed),
                &summary.scan_id[..8],
            )))
        }
        Err(gage_scan::runner::RunError::Emitted) => Err(DialogError::Failed(
            "Scan completed with errors, see above for details".to_string(),
        )),
        Err(gage_scan::runner::RunError::Canceled) => Err(DialogError::Canceled),
        Err(e) => Err(DialogError::Other(
            anyhow::anyhow!("{e}").context("scan runner"),
        )),
    }
}

/// Encoded project slugs recorded in the session corpus.
async fn known_projects() -> anyhow::Result<std::collections::HashSet<String>> {
    use datafusion::arrow::array::{Array, StringArray};

    let ctx = gage_query::create_context_default().await;
    let batches = ctx
        .sql("SELECT DISTINCT project FROM session")
        .await?
        .collect()
        .await?;
    let mut out = std::collections::HashSet::new();
    for batch in &batches {
        let col = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("session.project should be a Utf8 column");
        for i in 0..batch.num_rows() {
            if !col.is_null(i) {
                out.insert(col.value(i).to_string());
            }
        }
    }
    Ok(out)
}

/// Resolve a `-p` value to an encoded project slug recorded in the
/// session corpus. A leading `-` names a slug exactly. A value with no
/// path separator is tried as a slug first: as the home-relative
/// abbreviation `gage session list` shows, then with a leading `-`
/// prepended. Anything else — or a slug miss — resolves as a path
/// (`~` against `home`, relative against `cwd`), canonicalized and
/// encoded. None when no candidate matches a recorded project.
fn resolve_project(
    value: &str,
    home: &Path,
    cwd: &Path,
    projects: &std::collections::HashSet<String>,
) -> Option<String> {
    if value.starts_with('-') {
        return projects.contains(value).then(|| value.to_string());
    }
    if !value.contains('/') && !value.starts_with('~') {
        let abbreviated = format!("{}-{value}", session::encode_project_dir(home));
        if projects.contains(&abbreviated) {
            return Some(abbreviated);
        }
        let full = format!("-{value}");
        if projects.contains(&full) {
            return Some(full);
        }
    }
    let expanded = if value == "~" {
        home.to_path_buf()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else {
        std::path::PathBuf::from(value)
    };
    let resolved = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let canonical = resolved.canonicalize().unwrap_or(resolved);
    let slug = session::encode_project_dir(&canonical);
    projects.contains(&slug).then_some(slug)
}

/// Slug with the home prefix stripped — the form `gage session list`
/// shows.
fn project_slug_display(slug: &str) -> String {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let prefix = format!("{}-", session::encode_project_dir(Path::new(&home)));
    slug.strip_prefix(&prefix).unwrap_or(slug).to_string()
}

/// Elapsed time since midnight local time, for the --today window.
/// `SessionListBuilder::since` takes a duration back from now, so the
/// local-midnight cutoff is expressed as that offset.
fn since_local_midnight() -> Duration {
    use chrono::{Local, NaiveTime};
    let now = Local::now();
    let midnight = now
        .with_time(NaiveTime::MIN)
        .earliest()
        .expect("midnight should map to a local time");
    (now - midnight)
        .to_std()
        .expect("now should not precede midnight")
}

/// Run the scan under the full-screen scan view. The runner and the
/// view run concurrently in this task: runner events are adapted onto
/// a channel the view consumes. The view lingers after the scan
/// finishes; closing it mid-scan cancels the run.
#[allow(clippy::too_many_arguments)]
async fn run_scan_tui(
    db: Arc<Mutex<gage_db::rusqlite::Connection>>,
    scan_id: String,
    scanners: Vec<Scanner<'_>>,
    slots: Vec<gage_scan::ScannerSlot>,
    selected: Arc<[SessionInfo]>,
    scan_ctx: Arc<ScanSessionContext>,
    jobs: usize,
    agent_jobs: usize,
    cancel: tokio_util::sync::CancellationToken,
    mut streams: ScanStreams,
) -> Result<gage_scan::event::RunSummary, gage_scan::runner::RunError> {
    use gage_tui::scan_view::{self, ScanSetup, SessionEntry, TaskId};

    let setup = ScanSetup {
        tasks: scanners
            .iter()
            .flat_map(|s| {
                let mut names: Vec<String> = s.def.tasks.keys().cloned().collect();
                names.sort();
                let scanner = s.def.name.clone();
                names.into_iter().map(move |task| TaskId {
                    scanner: scanner.clone(),
                    task,
                })
            })
            .collect(),
        sessions: {
            let store = gage_query::default_index_store();
            selected
                .iter()
                .map(|s| SessionEntry {
                    id: s.id.clone(),
                    title: session_title(&store, s),
                    path: s.src.clone(),
                })
                .collect()
        },
    };

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    // Set when the runner returns; the reconcile poll exits on its
    // next tick without re-polling — the finish handler has already
    // sent the final reconcile.
    let scan_done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Agent session time bounds, cached across reconcile passes
    let agent_times = Arc::new(Mutex::new(AgentTimes::default()));
    let session_displays = Arc::new(Mutex::new(SessionDisplays::default()));

    // Reconcile notes/issues from the db once a second while the scan
    // runs — the same read the historical loader uses.
    let results_poll = {
        let tx = tx.clone();
        let db = db.clone();
        let cancel = cancel.clone();
        let scan_id = scan_id.clone();
        let scan_done = Arc::clone(&scan_done);
        let agent_times = Arc::clone(&agent_times);
        let session_displays = Arc::clone(&session_displays);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        if scan_done.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        if tx
                            .send(reconcile_results(&db, &scan_id, &agent_times, &session_displays))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
    };

    let mut print_buf = String::new();
    let event_tx = tx.clone();
    let scan_fut = async {
        let result = gage_scan::runner::run(
            db.clone(),
            scan_id.clone(),
            scanners,
            slots,
            selected,
            scan_ctx,
            jobs,
            agent_jobs,
            cancel.clone(),
            |event| {
                capture_event(&mut streams, &event);
                forward_scan_event(event, &event_tx, &mut print_buf);
            },
        )
        .await;
        // Finish handling: stop the poll, send one final reconcile so
        // the UI is current, then announce completion.
        scan_done.store(true, std::sync::atomic::Ordering::Relaxed);
        send_view_event(
            &event_tx,
            reconcile_results(&db, &scan_id, &agent_times, &session_displays),
        );
        send_view_event(&event_tx, scan_view::Event::Finished);
        result
    };

    let mut model = scan_view::ScanModel::new(setup);
    model.scan_id = short_uuid(&scan_id).to_string();
    model.out_path = Some(ScanStreams::out_path(&scan_id));
    let ui_cancel = cancel.clone();
    let run_cancel = cancel.clone();
    let ui_fut = async move {
        // The in-view cancel request cancels the run; the view stays up
        // and closes its Canceling dialog on the runner's Finished event.
        let result = scan_view::run(model, rx, move || run_cancel.cancel()).await;
        // Closing the view mid-scan stops the run; after the scan
        // completes this is a no-op.
        ui_cancel.cancel();
        result
    };

    let (scan_result, ui_result) = tokio::join!(scan_fut, ui_fut);
    if let Err(e) = results_poll.await
        && !e.is_cancelled()
    {
        panic!("results poll joined cleanly: {e}");
    }
    ui_result?;
    scan_result
}

/// Resolve a session's display title the same way the `session` query
/// table does: summary cache first, deriving (and re-caching) on a
/// miss. Falls back to the project name when the session has no title.
fn session_title(store: &gage_index::IndexStore, s: &SessionInfo) -> String {
    let title = match store.session_summary(&s.id, s.mtime) {
        Some(summary) => summary.title,
        None => match gage_index::derive_session(&s.id, &s.src) {
            Ok(d) => {
                if let Err(e) = store.put_session_summary(&s.id, &d.summary) {
                    tracing::warn!(session_id = %s.id, "failed to write summary cache: {e}");
                }
                d.summary.title
            }
            Err(e) => {
                tracing::warn!(session_id = %s.id, "session summary unavailable: {e}");
                None
            }
        },
    };
    title.unwrap_or_else(|| s.project_name().into_owned())
}

/// Tee a runner event into the scan's capture streams.
fn capture_event(streams: &mut ScanStreams, event: &gage_scan::event::ScanEvent) {
    use gage_scan::event::ScanEvent;
    match event {
        ScanEvent::Print { s } => streams.out(s),
        ScanEvent::Println { s } => streams.out_line(s),
        ScanEvent::Warning {
            scanner,
            task,
            message,
        } => streams.err_line(&format!(
            "warning: {}: {message}",
            task_display(scanner, task)
        )),
        ScanEvent::TaskFailed {
            scanner,
            task,
            message,
        } => {
            streams.err_line(&format!("error: {}", task_display(scanner, task)));
            streams.err_line(message);
        }
        ScanEvent::Status(_) => {}
    }
}

fn forward_scan_event(
    event: gage_scan::event::ScanEvent,
    tx: &tokio::sync::mpsc::UnboundedSender<gage_tui::scan_view::Event>,
    print_buf: &mut String,
) {
    use gage_scan::event::ScanEvent;
    use gage_tui::scan_view::{Event, RunningTask, TaskId};

    let ev = match event {
        ScanEvent::Status(s) => Event::Status {
            total: s.total,
            progress: s.progress,
            running: s
                .workers
                .iter()
                .filter_map(|w| w.current.as_ref())
                .map(|t| RunningTask {
                    id: TaskId {
                        scanner: t.scanner.clone(),
                        task: t.task.clone(),
                    },
                    progress: t.progress,
                })
                .collect(),
        },
        ScanEvent::Print { s } => {
            print_buf.push_str(&s);
            while let Some(i) = print_buf.find('\n') {
                let line: String = print_buf.drain(..=i).collect();
                send_view_event(tx, Event::Log(line.trim_end_matches('\n').to_string()));
            }
            return;
        }
        ScanEvent::Println { s } => Event::Log(s),
        ScanEvent::TaskFailed {
            scanner,
            task,
            message,
        } => Event::Failed {
            scanner,
            task,
            message,
        },
        ScanEvent::Warning {
            scanner,
            task,
            message,
        } => Event::Warning {
            scanner,
            task,
            message,
        },
    };
    send_view_event(tx, ev);
}

/// A send failure means the view exited and dropped the receiver; the
/// run is being cancelled and there is nowhere left to report to.
fn send_view_event(
    tx: &tokio::sync::mpsc::UnboundedSender<gage_tui::scan_view::Event>,
    ev: gage_tui::scan_view::Event,
) {
    if tx.send(ev).is_err() {}
}

/// Remove a deleted scan's log files (all streams, compressed or not).
/// Absent files are the normal case for scans predating log capture.
fn remove_scan_logs(scan_id: &str) {
    let dir = gage_log::role_dir("scan");
    for ext in ["log", "out", "err"] {
        for name in [format!("{scan_id}.{ext}"), format!("{scan_id}.{ext}.gz")] {
            match std::fs::remove_file(dir.join(&name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("warning: failed to remove log {name}: {e}"),
            }
        }
    }
}

fn all_issue_ids(conn: &gage_db::rusqlite::Connection) -> std::collections::HashSet<String> {
    let filters = gage_db::issue::IssueFilters {
        status: gage_db::issue::IssueStatusFilter::Any,
        ..Default::default()
    };
    gage_db::issue::find(conn, &filters)
        .map(|issues| issues.into_iter().map(|i| i.id).collect())
        .unwrap_or_default()
}

fn new_issues_since(
    conn: &gage_db::rusqlite::Connection,
    before: &std::collections::HashSet<String>,
) -> Vec<gage_db::issue::Issue> {
    let filters = gage_db::issue::IssueFilters {
        status: gage_db::issue::IssueStatusFilter::Any,
        ..Default::default()
    };
    gage_db::issue::find(conn, &filters)
        .map(|issues| {
            issues
                .into_iter()
                .filter(|i| !before.contains(&i.id))
                .collect()
        })
        .unwrap_or_default()
}

fn render_issues_remark(issues: &[gage_db::issue::Issue]) -> Result<(), DialogError> {
    if issues.is_empty() {
        cli::log::remark(style("No issues reported").italic().to_string())?;
        return Ok(());
    }

    let mut lines = String::new();
    for issue in issues {
        let title = issue.title.lines().next().unwrap_or("").trim();
        lines.push('\n');
        lines.push_str(&format!(
            "{} - {}",
            style(short_uuid(&issue.id)).yellow(),
            truncate(title, 77)
        ));
    }
    cli::log::remark(format!(
        "Issues\n{lines}\n\n{}",
        style("Run 'gage issue list' for details").italic().dim()
    ))?;
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

fn list_scanners(registry: &ScannerRegistry) {
    let header: Vec<String> = ["Scanner", "Groups", "Description"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error reading cwd: {e}");
            std::process::exit(1);
        }
    };
    let config = match gage_core::config::load_merged(&cwd) {
        Ok((c, _)) => c,
        Err(e) => {
            eprintln!("Error reading config: {e}");
            std::process::exit(1);
        }
    };

    let mut defs = registry.list_visible();
    defs.sort_by(|a, b| {
        let a_enabled = config.is_scanner_enabled(&a.name);
        let b_enabled = config.is_scanner_enabled(&b.name);
        b_enabled.cmp(&a_enabled).then_with(|| a.name.cmp(&b.name))
    });

    let rows: Vec<Vec<String>> = defs
        .into_iter()
        .map(|d| {
            let groups = style(d.groups.join(", ")).dim().to_string();
            if config.is_scanner_enabled(&d.name) {
                vec![
                    style(&d.name).yellow().to_string(),
                    groups,
                    style(&d.description).dim().to_string(),
                ]
            } else {
                vec![
                    style(format!("{} (disabled)", d.name)).dim().to_string(),
                    groups,
                    style(&d.description).dim().to_string(),
                ]
            }
        })
        .collect();

    let term_width = console::Term::stdout().size().1 as usize;
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .with(
            Width::wrap(term_width)
                .keep_words(true)
                .priority(Priority::max(true)),
        )
        .modify(Rows::first(), s::tty(Color::FG_BRIGHT_YELLOW))
        .to_string();
    println!("{table}");
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;

    use super::resolve_project;

    const HOME: &str = "/home/tester";
    const CWD: &str = "/work";

    fn resolve(value: &str, known: &[&str]) -> Option<String> {
        let projects: HashSet<String> = known.iter().map(|s| s.to_string()).collect();
        resolve_project(value, Path::new(HOME), Path::new(CWD), &projects)
    }

    #[test]
    fn full_slug_matches_exactly() {
        assert_eq!(
            resolve("-home-tester-Code-gage", &["-home-tester-Code-gage"]).as_deref(),
            Some("-home-tester-Code-gage")
        );
    }

    #[test]
    fn unknown_full_slug_is_none() {
        assert_eq!(
            resolve("-home-tester-nope", &["-home-tester-Code-gage"]),
            None
        );
    }

    #[test]
    fn abbreviation_prepends_home_slug() {
        assert_eq!(
            resolve("Code-gage", &["-home-tester-Code-gage"]).as_deref(),
            Some("-home-tester-Code-gage")
        );
    }

    #[test]
    fn slug_missing_leading_dash_matches() {
        assert_eq!(
            resolve("home-tester-Code-gage", &["-home-tester-Code-gage"]).as_deref(),
            Some("-home-tester-Code-gage")
        );
    }

    #[test]
    fn tilde_path_resolves_against_home() {
        assert_eq!(
            resolve("~/proj", &["-home-tester-proj"]).as_deref(),
            Some("-home-tester-proj")
        );
    }

    #[test]
    fn relative_path_resolves_against_cwd() {
        assert_eq!(
            resolve("proj", &["-work-proj"]).as_deref(),
            Some("-work-proj")
        );
    }

    #[test]
    fn abbreviation_wins_over_path() {
        // "proj" matches both the abbreviation and a cwd-relative path;
        // the slug interpretation is checked first
        assert_eq!(
            resolve("proj", &["-home-tester-proj", "-work-proj"]).as_deref(),
            Some("-home-tester-proj")
        );
    }

    #[test]
    fn no_match_is_none() {
        assert_eq!(resolve("nonexistent", &["-home-tester-Code-gage"]), None);
    }
}
