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

use gage_claude::session::{self, SessionInfo, SessionListBuilder};
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
    scan_id: String,
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
    #[arg(value_name = "SESSION", conflicts_with_all = ["limit", "days", "all", "sample"])]
    sessions: Vec<String>,

    /// Scanner to run (repeatable)
    #[arg(short, long = "scanner", value_name = "NAME")]
    scanners: Vec<String>,

    /// Scanner file to run (repeatable)
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    files: Vec<String>,

    /// Scan most recent N sessions
    #[arg(short = 'n', long, value_name = "N", conflicts_with_all = ["days", "all", "sample"])]
    limit: Option<usize>,

    /// Scan N sessions selected at random
    ///
    /// Samples from sessions modified in the past 30 days, or the
    /// window given with --days.
    #[arg(short = 'r', long, value_name = "N", conflicts_with = "all")]
    sample: Option<usize>,

    /// Scan sessions modified in past N days (default 30)
    #[arg(short, long, value_name = "N", conflicts_with = "all")]
    days: Option<u32>,

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
        conflicts_with_all = ["sessions", "scanners", "files", "limit", "days", "all", "sample"]
    )]
    rerun: Option<String>,

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

    let header: Vec<String> = [
        "Id", "Tasks", "Sessions", "Notes", "Issues", "Errors", "Duration", "Created",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let mut rows: Vec<Vec<String>> = Vec::with_capacity(show);
    for run in runs.iter().take(show) {
        match list_row(&conn, run) {
            Ok(row) => rows.push(row),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }

    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), Color::FG_BRIGHT_YELLOW)
        .modify(Columns::first().not(Rows::first()), Color::FG_BRIGHT_YELLOW)
        .modify(Columns::new(1..6), Alignment::right())
        .modify(Columns::last().not(Rows::first()), s::dim())
        .to_string();
    println!("{table}");

    args.limit.print_summary(show, total, "scan run");
}

fn list_row(conn: &gage_db::rusqlite::Connection, run: &scan::Scan) -> anyhow::Result<Vec<String>> {
    let tasks = scanner_task_outcomes(conn, &run.id)?;
    let errors = tasks
        .iter()
        .filter(|(_, t)| t.status == scan::TaskStatus::Failed)
        .count();
    let sessions = scan::session_ids_for_scan(conn, &run.id)?.len();
    let notes = scan::note_ids_for_scan(conn, &run.id)?.len();
    let issues = scan::issue_ids_for_scan(conn, &run.id)?.len();
    let duration = scan_summary(run)?
        .map(|s| crate::human::format_duration(Duration::from_millis(s.elapsed_ms)))
        .unwrap_or_default();
    Ok(vec![
        short_uuid(&run.id).to_string(),
        tasks.len().to_string(),
        sessions.to_string(),
        notes.to_string(),
        issues.to_string(),
        errors.to_string(),
        duration,
        crate::human::format_elapsed_ms(run.created),
    ])
}

/// Task outcomes recorded for a scan, paired with the scanner that ran
/// them. A scanner that never recorded metadata (interrupted scan)
/// contributes no tasks.
fn scanner_task_outcomes(
    conn: &gage_db::rusqlite::Connection,
    scan_id: &str,
) -> anyhow::Result<Vec<(String, scan::TaskOutcome)>> {
    let mut outcomes = Vec::new();
    for scanner in scan::get_scanners_for_scan(conn, scan_id)? {
        let Some(meta) = scanner.metadata.as_deref() else {
            continue;
        };
        let recorded: scan::ScannerTasks = serde_json::from_str(meta)?;
        outcomes.extend(
            recorded
                .tasks
                .into_iter()
                .map(|t| (scanner.scanner_name.clone(), t)),
        );
    }
    Ok(outcomes)
}

/// Run summary from `scan.metadata`. None when the scan never
/// completed.
fn scan_summary(run: &scan::Scan) -> anyhow::Result<Option<scan::ScanSummary>> {
    Ok(run
        .metadata
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?)
}

async fn view(args: ScanViewArgs) {
    let conn = db::open_db().unwrap();
    let model = match load_scan_model(&conn, &args.scan_id) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("gage scan view: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = gage_tui::scan_view::view(model).await {
        eprintln!("gage scan view: {e}");
        std::process::exit(1);
    }
}

/// Assemble a [`ScanModel`] for a completed scan from the db: task
/// outcomes from `scan_scanner.metadata`, the run summary from
/// `scan.metadata`, and sessions/notes/issues from the scan's edge
/// tables. A scan that never completed has no metadata; its tasks
/// table is empty and the header shows no progress.
fn load_scan_model(
    conn: &gage_db::rusqlite::Connection,
    prefix: &str,
) -> anyhow::Result<gage_tui::scan_view::ScanModel> {
    use gage_tui::scan_view::{ScanModel, SessionItem, TaskId, TaskItem, TaskState};
    use std::collections::HashMap;

    let run = scan::get_scan(conn, prefix)?;
    let summary = scan_summary(&run)?;

    let tasks: Vec<TaskItem> = scanner_task_outcomes(conn, &run.id)?
        .into_iter()
        .map(|(scanner, t)| TaskItem {
            id: TaskId {
                scanner,
                task: t.name,
            },
            state: match t.status {
                scan::TaskStatus::Completed => TaskState::Completed,
                scan::TaskStatus::Failed => TaskState::Error,
                scan::TaskStatus::Skipped => TaskState::Skipped,
            },
            elapsed: t.elapsed_ms.map(Duration::from_millis),
            started: None,
        })
        .collect();
    let errors = tasks.iter().filter(|t| t.state == TaskState::Error).count();

    let results = load_scan_results(conn, &run.id)?;
    let counts: HashMap<&str, (usize, usize)> = results
        .sessions
        .iter()
        .map(|c| (c.id.as_str(), (c.notes, c.issues)))
        .collect();

    let store = gage_query::default_index_store();
    let paths: HashMap<String, std::path::PathBuf> = session::ls_sessions().into_iter().collect();
    let mut sessions: Vec<SessionItem> = scan::session_ids_for_scan(conn, &run.id)?
        .into_iter()
        .map(|id| {
            let title = stat_session(&paths, &id)
                .map(|info| session_title(&store, &info))
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
        out_path: Some(ScanStreams::out_path(&run.id)),
        total: summary.as_ref().map(|s| s.total).unwrap_or(0),
        progress: summary
            .as_ref()
            .map(|s| s.completed + s.failed)
            .unwrap_or(0),
        notes: results.notes,
        issues: results.issues,
        errors,
        finished: true,
        elapsed: summary
            .as_ref()
            .map(|s| Duration::from_millis(s.elapsed_ms)),
        tasks,
        sessions,
    })
}

/// One reconcile pass for the live view: load the scan's results from
/// the db as a Results event, or a Log event when the read fails.
fn reconcile_results(
    db: &Arc<Mutex<gage_db::rusqlite::Connection>>,
    scan_id: &str,
) -> gage_tui::scan_view::Event {
    let results = {
        let conn = db.lock().unwrap();
        load_scan_results(&conn, scan_id)
    };
    match results {
        Ok(r) => gage_tui::scan_view::Event::Results {
            notes: r.notes,
            issues: r.issues,
            sessions: r.sessions,
        },
        Err(e) => gage_tui::scan_view::Event::Log(format!("results refresh failed: {e}")),
    }
}

/// Notes, issues, and per-session counts recorded for a scan — shared
/// between the historical loader and the live view's reconcile poll.
struct ScanResults {
    notes: Vec<gage_tui::scan_view::NoteItem>,
    issues: Vec<gage_tui::scan_view::IssueItem>,
    sessions: Vec<gage_tui::scan_view::SessionCounts>,
}

fn load_scan_results(
    conn: &gage_db::rusqlite::Connection,
    scan_id: &str,
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
            name: None,
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
    // An issue attributes to every session its evidence touches, once
    // per session.
    for issue in &issues {
        let mut sessions: HashSet<String> = HashSet::new();
        for note in gage_db::issue::related_notes(conn, &issue.id)? {
            if let gage_db::target::NoteTarget::Session(t) = &note.target {
                sessions.insert(t.session_id.clone());
            }
        }
        for session_id in sessions {
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
                kind: ev.event.type_str().to_string(),
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
            author: i.author.clone(),
            created: gage_core::datetime::ms_to_iso8601(i.created),
            description: i.description.clone(),
            evidence,
            events,
        });
    }

    Ok(ScanResults {
        issues: issue_items,
        notes: notes
            .iter()
            .map(|n| NoteItem {
                id: n.id.clone(),
                name: n.name.clone(),
                value: crate::cmd_note::format_value_cell(&n.value),
                value_full: crate::cmd_note::format_value(&n.value),
                target: n.target.to_uri(),
                target_cell: crate::cmd_note::shorten_target(&n.target),
                author: n.author.clone(),
                created: gage_core::datetime::ms_to_iso8601(n.created),
                explanation: n.explanation.clone(),
            })
            .collect(),
        sessions: counts
            .into_iter()
            .map(|(id, (notes, issues))| SessionCounts { id, notes, issues })
            .collect(),
    })
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
    let mut scanners: Vec<String> = scan::get_scanners_for_scan(conn, &run.id)?
        .into_iter()
        .map(|s| s.scanner_name)
        .collect();
    scanners.sort();
    scanners.dedup();
    if scanners.is_empty() {
        anyhow::bail!("scan {} has no scanners", short_uuid(&run.id));
    }
    let sessions = scan::session_ids_for_scan(conn, &run.id)?;
    if sessions.is_empty() {
        anyhow::bail!("scan {} has no sessions", short_uuid(&run.id));
    }
    Ok((scanners, sessions))
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
    let defs = registry.list_enabled(&config);
    let mut names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    names.sort();

    let selected_names: Vec<String> = if args.scanners.is_empty() && !args.yes {
        let mut prompt = cli::multiselect("Scanners");
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
    } else if args.scanners.is_empty() {
        names.iter().map(|n| n.to_string()).collect()
    } else {
        for name in &args.scanners {
            let bare = name.split("#{").next().unwrap();
            if !registry.is_known(bare) {
                cli::log::error(format!("Unknown scanner: {bare}"))?;
                return Err(DialogError::Canceled);
            }
        }
        args.scanners.clone()
    };

    if !args.scanners.is_empty() || args.yes {
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

    let scanners: Vec<Scanner<'_>> = {
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

    // Session selection
    let sessions = if let Some(resolved) = explicit_sessions {
        let session_lines: String = resolved
            .iter()
            .map(|(id, _)| format!("\n{}", style(id).dim()))
            .collect();
        cli::log::step(format!("Sessions{session_lines}"))?;
        resolved
    } else {
        let days = if args.all || args.limit.is_some() {
            None
        } else {
            Some(args.days.unwrap_or(30))
        };

        let label = if args.all {
            "all".to_string()
        } else if let Some(n) = args.limit {
            format!("{n} most recent")
        } else {
            let d = days.unwrap();
            let window = format!("last {d} day{}", if d == 1 { "" } else { "s" });
            match args.sample {
                Some(n) => format!("{n} sampled from {window}"),
                None => window,
            }
        };
        cli::log::step(format!("Sessions\n{}", style(label).dim()))?;

        let mut builder = SessionListBuilder::new();
        if let Some(d) = days {
            builder = builder.since(Duration::from_secs(u64::from(d) * 86_400));
        }
        if let Some(n) = args.limit {
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

    let db = Arc::new(Mutex::new(db::open_db().unwrap()));

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
                        eprintln!("error: {scanner}::{task}");
                        for line in message.lines() {
                            eprintln!("{line}");
                        }
                    }
                    gage_scan::event::ScanEvent::Warning {
                        scanner,
                        task,
                        message,
                    } => {
                        eprintln!("warning: {scanner}::{task}: {message}");
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

/// Run the scan under the full-screen scan view. The runner and the
/// view run concurrently in this task: runner events are adapted onto
/// a channel the view consumes. The view lingers after the scan
/// finishes; closing it mid-scan cancels the run.
#[allow(clippy::too_many_arguments)]
async fn run_scan_tui(
    db: Arc<Mutex<gage_db::rusqlite::Connection>>,
    scan_id: String,
    scanners: Vec<Scanner<'_>>,
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

    // Reconcile notes/issues from the db once a second while the scan
    // runs — the same read the historical loader uses.
    let results_poll = {
        let tx = tx.clone();
        let db = db.clone();
        let cancel = cancel.clone();
        let scan_id = scan_id.clone();
        let scan_done = Arc::clone(&scan_done);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        if scan_done.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        if tx.send(reconcile_results(&db, &scan_id)).is_err() {
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
        send_view_event(&event_tx, reconcile_results(&db, &scan_id));
        send_view_event(&event_tx, scan_view::Event::Finished);
        result
    };

    let mut model = scan_view::ScanModel::new(setup);
    model.out_path = Some(ScanStreams::out_path(&scan_id));
    let ui_cancel = cancel.clone();
    let ui_fut = async move {
        let result = scan_view::run(model, rx).await;
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
        } => streams.err_line(&format!("warning: {scanner}::{task}: {message}")),
        ScanEvent::TaskFailed {
            scanner,
            task,
            message,
        } => {
            streams.err_line(&format!("error: {scanner}::{task}"));
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
    use gage_tui::scan_view::{Event, TaskId};

    let ev = match event {
        ScanEvent::Status(s) => Event::Status {
            total: s.total,
            progress: s.progress,
            running: s
                .workers
                .iter()
                .filter_map(|w| w.current.as_ref())
                .map(|t| TaskId {
                    scanner: t.scanner.clone(),
                    task: t.task.clone(),
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
        name: None,
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
        name: None,
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
    let header: Vec<String> = ["Scanner", "Description"]
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
            if config.is_scanner_enabled(&d.name) {
                vec![
                    style(&d.name).yellow().to_string(),
                    style(&d.description).dim().to_string(),
                ]
            } else {
                vec![
                    style(format!("{} (disabled)", d.name)).dim().to_string(),
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
        .modify(Rows::first(), Color::FG_BRIGHT_YELLOW)
        .to_string();
    println!("{table}");
}
