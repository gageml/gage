use std::num::IntErrorKind;
use std::sync::{Arc, Mutex};

use clap::{Args, Subcommand};
use cliclack as cli;
use console::style;
use tabled::{
    Table,
    settings::{
        Color, Style, Width,
        object::{Columns, Object, Rows},
        peaker::Priority,
    },
};

use gage_claude::session;
use gage_core::uuid::short_uuid;
use gage_db::{db, scan};
use gage_scan::scanner::{Scanner, ScannerRegistry};

use crate::dialog::{self, DialogError, DialogResult};
use crate::style as s;

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
    /// Show details for a scan run
    Show(ScanShowArgs),
    /// Delete scan runs and associated notes
    Delete(ScanDeleteArgs),
}

#[derive(Args)]
pub struct ScanListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,
}

#[derive(Args)]
pub struct ScanShowArgs {
    /// Scan ID (or prefix)
    scan_id: String,
}

#[derive(Args)]
pub struct ScanDeleteArgs {
    /// Scan run IDs (prefix match)
    ids: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct ScanRunArgs {
    /// Sessions to scan (ID or prefix). Repeatable
    #[arg(value_name = "SESSION", conflicts_with_all = ["all", "limit"])]
    sessions: Vec<String>,

    /// Scanners to run (repeatable)
    #[arg(short, long = "scanner", value_name = "NAME")]
    scanners: Vec<String>,

    /// Scanner files to run (repeatable). Like `-s`, including any
    /// `-f` replaces the default "run all enabled scanners".
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    files: Vec<String>,

    /// Scan all sessions
    #[arg(short, long, conflicts_with_all = ["limit", "sessions"])]
    all: bool,

    /// Scan most recent N sessions (default: 20)
    #[arg(short, long, conflicts_with_all = ["all", "sessions"])]
    limit: Option<usize>,

    /// Skip confirmation prompts
    #[arg(short, long)]
    yes: bool,

    /// Maximum concurrent task workers (defaults to number of CPUs)
    #[arg(short, long, value_name = "N")]
    jobs: Option<usize>,

    /// Suppress the interactive progress display. Per-task bars and
    /// the summary bar are not shown; scanner stdout flows directly.
    #[arg(long)]
    no_progress: bool,

    /// List registered scanners and exit
    #[arg(long)]
    list_scanners: bool,

    /// Enable a scanner in settings and exit (repeatable)
    #[arg(long = "enable", value_name = "NAME", conflicts_with_all = ["sessions", "scanners", "files", "all", "limit", "jobs", "no_progress", "list_scanners"])]
    enable: Vec<String>,

    /// Disable a scanner in settings and exit (repeatable)
    #[arg(long = "disable", value_name = "NAME", conflicts_with_all = ["sessions", "scanners", "files", "all", "limit", "jobs", "no_progress", "list_scanners"])]
    disable: Vec<String>,
}

pub async fn run(args: ScanArgs) {
    match args.command {
        Some(ScanCommand::List(a)) => list(a),
        Some(ScanCommand::Show(a)) => show(a),
        Some(ScanCommand::Delete(a)) => delete(a),
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

    let header: Vec<String> = ["Id", "Scanners", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let rows: Vec<Vec<String>> = runs
        .iter()
        .take(show)
        .map(|run| {
            let mut scanners = scan::get_scanners_for_scan(&conn, &run.id)
                .unwrap_or_default()
                .iter()
                .map(|s| s.scanner_name.clone())
                .collect::<Vec<_>>();
            scanners.sort();
            let scanners = scanners.join(", ");

            vec![
                short_uuid(&run.id).to_string(),
                scanners,
                crate::human::format_elapsed_ms(run.created),
            ]
        })
        .collect();

    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), Color::FG_BRIGHT_YELLOW)
        .modify(Columns::first().not(Rows::first()), Color::FG_BRIGHT_YELLOW)
        .modify(Columns::new(2..3).not(Rows::first()), s::dim())
        .to_string();
    println!("{table}");

    args.limit.print_summary(show, total, "scan run");
}

fn show(args: ScanShowArgs) {
    let conn = db::open_db().unwrap();
    let run = match scan::get_scan(&conn, &args.scan_id) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let scans = scan::get_scanners_for_scan(&conn, &run.id).unwrap_or_default();

    println!(
        "{} {}",
        style("Run").bold(),
        style(short_uuid(&run.id)).yellow(),
    );
    println!(
        "  created: {}",
        gage_core::datetime::ms_to_iso8601(run.created)
    );

    if scans.is_empty() {
        println!("  (no scanners)");
        return;
    }

    let header: Vec<String> = ["Id", "Scanner", "Version"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let rows: Vec<Vec<String>> = scans
        .iter()
        .map(|s| {
            vec![
                short_uuid(&s.id).to_string(),
                s.scanner_name.clone(),
                s.scanner_version.clone(),
            ]
        })
        .collect();

    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), Color::FG_BRIGHT_YELLOW)
        .modify(Columns::new(2..3).not(Rows::first()), s::dim())
        .to_string();
    println!("{table}");
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

async fn run_scan(mut args: ScanRunArgs) {
    let mut registry = ScannerRegistry::load();

    if !args.enable.is_empty() || !args.disable.is_empty() {
        apply_enable_disable(&registry, &args.enable, &args.disable);
        return;
    }

    if args.list_scanners {
        list_scanners(&registry);
        return;
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

    dialog::run_async("Scan sessions", || {
        run_dialog(args, registry, explicit_sessions)
    })
    .await;
}

async fn run_dialog(
    args: ScanRunArgs,
    registry: ScannerRegistry,
    explicit_sessions: Option<Vec<(String, std::path::PathBuf)>>,
) -> Result<DialogResult, DialogError> {
    // Scanner selection — default set excludes disabled scanners.
    // Explicit `-s name` (handled below) still runs disabled scanners.
    let cwd = std::env::current_dir()
        .map_err(|e| DialogError::Other(std::io::Error::other(e.to_string())))?;
    let (config, _) = gage_core::config::load_merged(&cwd)
        .map_err(|e| DialogError::Other(std::io::Error::other(e.to_string())))?;
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
        let session_limit: Option<usize> = if args.all {
            cli::log::step(format!("Limit\n{}", style("all").dim()))?;
            None
        } else if let Some(n) = args.limit {
            cli::log::step(format!("Limit\n{}", style(n).dim()))?;
            Some(n)
        } else if args.yes {
            let n = 20;
            cli::log::step(format!("Limit\n{}", style(n).dim()))?;
            Some(n)
        } else {
            let input: String = cli::input("Limit")
                .default_input("20")
                .placeholder("number or 'all' (default is 20)")
                .validate(|v: &String| want_positive_number_or_all(v))
                .interact()?;
            let trimmed = input.trim();
            if trimmed.eq_ignore_ascii_case("all") {
                None
            } else {
                Some(trimmed.parse::<usize>().unwrap())
            }
        };
        let mut sessions = session::ls_sessions();
        if let Some(n) = session_limit {
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

    let mut progress = if args.no_progress {
        None
    } else {
        Some(crate::scan_progress::ProgressUi::new())
    };
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

    let result = gage_scan::runner::run(
        db.clone(),
        scanners,
        sessions,
        jobs,
        cancel.clone(),
        |event| {
            if let Some(ui) = progress.as_mut() {
                ui.handle(event);
            } else {
                // --no-progress: route scanner stdout straight through
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
            }
        },
    )
    .await;
    let elapsed = started.elapsed();

    cancel.cancel();
    if let Err(e) = signal_task.await
        && !e.is_cancelled()
    {
        panic!("signal task joined cleanly: {e}");
    }

    if let Some(ui) = progress {
        ui.finish();
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
        Err(e) => Err(DialogError::Other(std::io::Error::other(e.to_string()))),
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

    let defs = registry.list_visible();

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

fn apply_enable_disable(registry: &ScannerRegistry, enable: &[String], disable: &[String]) {
    let mut errors = 0;
    for name in enable.iter().chain(disable.iter()) {
        if !registry.is_known(name) {
            eprintln!("Unknown scanner: {name}");
            errors += 1;
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let user_path = gage_core::config::user_config_path();
    let mut config = match gage_core::config::Config::load_from(&user_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error reading config: {e}");
            std::process::exit(1);
        }
    };

    let mut changed = 0;
    for name in enable {
        let before = config.scanners.disable.len();
        config.scanners.disable.retain(|n| n != name);
        if config.scanners.disable.len() != before {
            changed += 1;
            println!("Enabled {name}");
        }
    }
    for name in disable {
        if !config.scanners.disable.iter().any(|n| n == name) {
            config.scanners.disable.push(name.clone());
            changed += 1;
            println!("Disabled {name}");
        }
    }

    if changed == 0 {
        println!("No changes");
        return;
    }

    if let Err(e) = config.save_to(&user_path) {
        eprintln!("Error writing config: {e}");
        std::process::exit(1);
    }
}

fn want_positive_number_or_all(s: &str) -> Result<(), &'static str> {
    const MSG: &str = "enter a positive number or 'all'";
    let s = s.trim();
    if s.eq_ignore_ascii_case("all") {
        return Ok(());
    }
    match s.parse::<usize>() {
        Ok(n) if n > 0 => Ok(()),
        Ok(_) => Err(MSG),
        Err(e) => match e.kind() {
            IntErrorKind::InvalidDigit
            | IntErrorKind::Empty
            | IntErrorKind::PosOverflow
            | IntErrorKind::NegOverflow
            | IntErrorKind::Zero => Err(MSG),
            kind => panic!("unexpected IntErrorKind: {kind:?}"),
        },
    }
}
