//! The `gage eval` command: run and view evals. The eval engine lives
//! in the `gage-eval` crate; this module is its command layer.

use clap::{Args, Subcommand};
use gage_core::style::IdHighlighter;
use gage_core::uuid::short_uuid;
use indicatif::{ProgressBar, ProgressStyle};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style, Width,
        object::{Columns, Object, Rows},
        peaker::Peaker,
    },
};

use gage_eval::{eval, run as runner, score, storage, tokens, view};

use crate::{limit, style};

#[derive(Subcommand)]
pub enum EvalCommand {
    /// Run an eval
    Run(RunArgs),
    /// List eval runs
    List(ListArgs),
    /// View an eval run report
    View(ViewArgs),
    /// Delete one or more eval runs
    Delete(DeleteArgs),
}

#[derive(Args)]
pub struct ListArgs {
    #[command(flatten)]
    limit: limit::LimitArgs,

    /// Filter to runs started within this duration (e.g. 1h, 30m, 7d)
    #[arg(short, long, value_parser = parse_duration)]
    since: Option<std::time::Duration>,
}

fn parse_duration(s: &str) -> Result<std::time::Duration, humantime::DurationError> {
    humantime::parse_duration(s)
}

#[derive(Args)]
pub struct DeleteArgs {
    /// Run UUIDs or unique prefixes. Each must match exactly one run
    run_ids: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct ViewArgs {
    /// Run UUID or unique prefix
    run_id: String,

    /// Rebuild report.md
    #[arg(long)]
    refresh: bool,

    /// Pick a test and view its session
    #[arg(short, long)]
    session: bool,
}

const DEFAULT_MODEL: &str = "sonnet";
const DEFAULT_EFFORT: &str = "low";
const DEFAULT_JUDGE_MODEL: &str = "sonnet";
const DEFAULT_JOBS: usize = 4;
const DEFAULT_SAMPLE_JOBS: usize = 4;

#[derive(Args)]
pub struct RunArgs {
    /// Tests to run (default: all)
    ///
    /// `*` matches everything; `eval/test` matches one test; a bare
    /// token matches that test-id in any eval, or every test in an eval
    /// of that name. `*` does not cross `/`. Prefix any spec with `!` to
    /// exclude.
    tests: Vec<String>,

    /// Print selected tests and exit
    #[arg(short, long = "list-tests")]
    list: bool,

    /// Model for tests
    #[arg(short, long, default_value = DEFAULT_MODEL)]
    model: String,

    /// Effort level for tests (low, medium, high, xhigh, max)
    #[arg(short, long, default_value = DEFAULT_EFFORT)]
    effort: String,

    /// Note recorded with the run
    ///
    /// Stored in manifest.json and shown in `gage eval list`. Useful for
    /// labeling what you were varying.
    #[arg(short, long)]
    note: Option<String>,

    /// Load evals from a directory instead of the repo's evals
    ///
    /// The directory holds eval `*.toml` files and a `fixtures/` subdir.
    /// Use for ad hoc tests staged outside source control (e.g. under
    /// ~/.gage/tmp/evals).
    #[arg(short = 'd', long, value_name = "DIR")]
    evals_dir: Option<std::path::PathBuf>,

    /// Concurrent tests
    #[arg(short, long, value_name = "N", default_value_t = DEFAULT_JOBS)]
    jobs: usize,

    /// Concurrent samples within a scanner test
    #[arg(long, value_name = "N", default_value_t = DEFAULT_SAMPLE_JOBS)]
    jobs_samples: usize,

    /// Judge model for scanner tests
    #[arg(long, value_name = "MODEL", default_value = DEFAULT_JUDGE_MODEL)]
    judge_model: String,

    /// Run without being prompted
    #[arg(short, long)]
    yes: bool,
}

pub async fn run(command: EvalCommand) {
    match command {
        EvalCommand::Run(args) => cmd_run(args),
        EvalCommand::List(args) => cmd_list(args),
        EvalCommand::View(args) => cmd_view(args).await,
        EvalCommand::Delete(args) => cmd_delete(args),
    }
}

async fn cmd_view(args: ViewArgs) {
    let run = match view::resolve(&args.run_id) {
        Ok(r) => r,
        Err(e) => {
            if let Some(amb) = e
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<view::AmbiguousError>())
            {
                eprintln!("ambiguous prefix `{}` matches multiple runs:", args.run_id);
                eprint!("{}", runs_table(&amb.matches));
            } else {
                eprintln!("{e}");
            }
            std::process::exit(1);
        }
    };
    if args.session {
        cmd_view_session(&run).await;
        return;
    }
    let path = match view::ensure_report(&run, args.refresh) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to build report: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = view::page(&path) {
        eprintln!("pager failed: {e}");
        std::process::exit(2);
    }
}

async fn cmd_view_session(run: &storage::RunSummary) {
    let session_id = match pick_test_session(run) {
        Ok(Some(id)) => id,
        Ok(None) => return,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let run_dir = storage::run_dir(&run.run_id);
    let projects = storage::claude_home(&run_dir).join("projects");
    // SAFETY: set_var is unsafe in edition 2024; the runtime's worker
    // threads exist, but nothing reads this variable concurrently — the
    // sole reader is the session load performed on this thread below.
    unsafe { std::env::set_var("CLAUDE_PROJECTS_DIR", &projects) };
    let options = gage_tui::ViewOptions { show_turns: true };
    if let Err(e) = gage_tui::session_view::run(&session_id, options).await {
        eprintln!("failed to view session: {e}");
        std::process::exit(2);
    }
}

/// Present a pick list of the run's tests, one row per test with a
/// session JSONL. Resolves to the selected test's session id; `None`
/// when canceled or when no test has a session.
fn pick_test_session(run: &storage::RunSummary) -> std::io::Result<Option<String>> {
    let run_dir = storage::run_dir(&run.run_id);
    let mut items: Vec<(String, String, String)> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for name in storage::test_names(&run_dir)? {
        let id = storage::session_path(&run_dir, &name)
            .and_then(|p| Some(p.file_stem()?.to_str()?.to_string()));
        let Some(id) = id else {
            missing.push(name);
            continue;
        };
        let glyph = match score::read_score(&run_dir, &name)? {
            Some(s) if s.passed => "✓ ",
            Some(_) => "✗ ",
            None => "  ",
        };
        items.push((id, format!("{glyph}{name}"), String::new()));
    }
    if !missing.is_empty() {
        eprintln!("no session for: {}", missing.join(", "));
    }
    if items.is_empty() {
        println!("No sessions found");
        return Ok(None);
    }
    let _sigint = SigintGuard::new();
    match cliclack::select("Select a test session")
        .items(&items)
        .max_rows(15)
        .filter_mode()
        .interact()
    {
        Ok(id) => Ok(Some(id)),
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(None),
        Err(e) => Err(e),
    }
}

/// RAII guard that ignores SIGINT for its lifetime and restores the
/// previous disposition on drop. The console crate detects Ctrl+C as a
/// 0x03 byte in raw mode and then calls `libc::raise(SIGINT)`; without
/// this guard the raised signal terminates the process before the
/// caller can act on the `ErrorKind::Interrupted` returned by the
/// prompt.
struct SigintGuard {
    prev: libc::sighandler_t,
}

impl SigintGuard {
    fn new() -> Self {
        let prev = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        Self { prev }
    }
}

impl Drop for SigintGuard {
    fn drop(&mut self) {
        unsafe {
            libc::signal(libc::SIGINT, self.prev);
        }
    }
}

fn cmd_delete(args: DeleteArgs) {
    if args.run_ids.is_empty() {
        eprintln!("provide at least one run UUID or prefix");
        std::process::exit(1);
    }
    let mut resolved: Vec<storage::RunSummary> = Vec::with_capacity(args.run_ids.len());
    for spec in &args.run_ids {
        match view::resolve(spec) {
            Ok(r) => resolved.push(r),
            Err(e) => {
                if let Some(amb) = e
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<view::AmbiguousError>())
                {
                    eprintln!("ambiguous prefix `{spec}` matches multiple runs:");
                    eprint!("{}", runs_table(&amb.matches));
                } else {
                    eprintln!("`{spec}`: {e}");
                }
                std::process::exit(1);
            }
        }
    }
    resolved.sort_by_key(|e| std::cmp::Reverse(e.started_at_ms));

    if args.yes {
        let deleted = delete_runs(&resolved);
        let plural = if deleted == 1 { "run" } else { "runs" };
        println!("Deleted {deleted} {plural}");
        return;
    }

    if let Err(e) = run_delete_dialog(&resolved) {
        eprintln!("{e}");
        std::process::exit(2);
    }
}

fn run_delete_dialog(runs: &[storage::RunSummary]) -> std::io::Result<()> {
    cliclack::intro(console::style("Delete runs").bold())?;
    cliclack::log::remark(runs_table(runs).trim_end())?;
    let confirmed = cliclack::confirm("Permanently delete? This cannot be undone.")
        .initial_value(false)
        .interact()?;
    if !confirmed {
        cliclack::outro_cancel("Canceled")?;
        return Ok(());
    }
    let deleted = delete_runs(runs);
    let plural = if deleted == 1 { "run" } else { "runs" };
    cliclack::outro(
        console::style(format!("Deleted {deleted} {plural}"))
            .green()
            .bright(),
    )?;
    Ok(())
}

fn delete_runs(runs: &[storage::RunSummary]) -> usize {
    let mut deleted = 0;
    for r in runs {
        let dir = storage::run_dir(&r.run_id);
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            eprintln!("warning: failed to delete {}: {e}", short_uuid(&r.run_id));
        } else {
            deleted += 1;
        }
    }
    deleted
}

fn cmd_run(args: RunArgs) {
    let root = match &args.evals_dir {
        Some(dir) => eval::Root::at(dir),
        None => eval::Root::repo(),
    };
    let all = match eval::load_all(&root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to load evals: {e}");
            std::process::exit(2);
        }
    };
    let tests = match eval::select(&all, &args.tests) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    if tests.is_empty() {
        eprintln!("No tests matched.");
        std::process::exit(1);
    }

    if let Err(missing) = eval::validate(&root, &tests) {
        eprintln!("missing fixtures:");
        for (test_id, fixture) in &missing {
            eprintln!("  {test_id}: fixture `{fixture}` not found");
        }
        std::process::exit(1);
    }

    if args.list {
        println!("Selected {} test(s):", tests.len());
        for t in &tests {
            println!("  {}", t.id());
        }
        return;
    }

    let has_prompt_tests = tests.iter().any(|t| !t.is_scanner());
    if has_prompt_tests && std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("ANTHROPIC_API_KEY is required");
        std::process::exit(1);
    }

    if let Err(e) = show_run_intro(&tests, &args) {
        eprintln!("{e}");
        std::process::exit(2);
    }
    if !args.yes {
        let confirmed = match cliclack::confirm("Continue?")
            .initial_value(true)
            .interact()
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        if !confirmed {
            cliclack::outro_cancel("Canceled").unwrap();
            return;
        }
    }

    let pb = ProgressBar::new(tests.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.magenta} {msg} [{elapsed_precise}] {bar:30.white/bright.black} ({pos}/{len})",
        )
        .expect("static template")
        .progress_chars("▬▬"),
    );
    pb.set_message("starting...");
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    let started = std::time::Instant::now();
    let mut error_count: u32 = 0;
    let mut passed: u32 = 0;
    let mut failed: u32 = 0;
    let config = runner::BatchConfig {
        model: &args.model,
        effort: &args.effort,
        note: args.note.as_deref(),
        root: &root,
        evals_dir: args.evals_dir.as_deref(),
        jobs: args.jobs,
        sample_jobs: args.jobs_samples,
        judge_model: &args.judge_model,
    };
    let result = match runner::run_batch(&tests, &config, |evt| match evt {
        runner::Event::Started(name) => pb.set_message(name.to_string()),
        runner::Event::TestFinished {
            name,
            exit_code,
            score,
        } => {
            pb.inc(1);
            let bar = console::style("│").bright().black();
            if exit_code != 0 {
                error_count += 1;
                // Scored tests report the exit code among their failed
                // checks; only unscored tests need a standalone line.
                if score.is_none() {
                    let msg = console::style(format!("  {name}  claude exited {exit_code}")).red();
                    pb.println(format!("{bar} {msg}"));
                }
            }
            if let Some(s) = score {
                if s.passed {
                    passed += 1;
                    pb.println(format!("{bar}   ✓ {name}"));
                } else {
                    failed += 1;
                    let msg = console::style(format!("✗ {name} FAILED")).red();
                    pb.println(format!("{bar}   {msg}"));
                    for m in s.matches.iter().filter(|m| !m.matched) {
                        let line = console::style(one_line(&m.pattern, 120)).red();
                        pb.println(format!("{bar}     {line}"));
                    }
                }
            }
        }
    }) {
        Ok(o) => {
            pb.finish_and_clear();
            o
        }
        Err(e) => {
            pb.finish_and_clear();
            eprintln!("run failed: {e}");
            std::process::exit(2);
        }
    };

    let elapsed = format_elapsed(started.elapsed());
    let scored = passed + failed;
    if scored > 0 {
        let pct = (passed as f64 / scored as f64 * 100.0).round() as u32;
        let plural = if scored == 1 { "test" } else { "tests" };
        eprintln!("{}", console::style("│").bright().black());
        cliclack::log::remark(format!("{passed}/{scored} {plural} passed ({pct}%)")).unwrap();
    }
    let run_id = short_uuid(&result.run_id);
    if error_count > 0 {
        cliclack::outro_cancel(format!(
            "Run {run_id} completed with errors (see above for details) in {elapsed}"
        ))
        .unwrap();
    } else {
        cliclack::outro(
            console::style(format!("Run {run_id} completed in {elapsed}"))
                .green()
                .bright(),
        )
        .unwrap();
    }
}

/// Collapse whitespace runs to single spaces and truncate to `max`
/// chars for one-line terminal display.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max).collect();
    out.push('…');
    out
}

fn show_run_intro(tests: &[&eval::Test], args: &RunArgs) -> std::io::Result<()> {
    cliclack::intro(console::style("Run eval").bold())?;

    let mut by_eval: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    for t in tests {
        by_eval
            .entry(t.eval.as_str())
            .or_default()
            .push(t.test_id());
    }
    let mut evals_line = String::from("Selected\n");
    for (name, ids) in &mut by_eval {
        ids.sort();
        let shown = ids.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
        let extra = ids.len().saturating_sub(3);
        let list = if extra > 0 {
            format!("{shown} ({extra} more)")
        } else {
            shown
        };
        let list = console::style(list).dim();
        evals_line.push_str(&format!("  {name}: {list}\n"));
    }
    cliclack::log::remark(evals_line.trim_end())?;

    if let Some(dir) = &args.evals_dir {
        cliclack::log::remark(format!("Evals dir: {}", dir.display()))?;
    }

    if tests.iter().any(|t| !t.is_scanner()) {
        let model = &args.model;
        let model_suffix = if model == DEFAULT_MODEL {
            " (default)"
        } else {
            ""
        };
        cliclack::log::remark(format!("Model: {model}{model_suffix}"))?;

        let effort = &args.effort;
        let effort_suffix = if effort == DEFAULT_EFFORT {
            " (default)"
        } else {
            ""
        };
        cliclack::log::remark(format!("Effort: {effort}{effort_suffix}"))?;
    }

    if tests.iter().any(|t| t.is_scanner()) {
        cliclack::log::remark(format!("Judge: {}", args.judge_model))?;
    }
    Ok(())
}

fn cmd_list(args: ListArgs) {
    let mut runs = match storage::list_runs() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("list failed: {e}");
            std::process::exit(2);
        }
    };
    if let Some(duration) = args.since {
        let cutoff = gage_core::datetime::now_ms() - duration.as_millis() as i64;
        runs.retain(|r| r.started_at_ms >= cutoff);
    }
    let total = runs.len();
    if total == 0 {
        println!("No runs found");
        return;
    }
    let show = args.limit.show_count(total);
    runs.truncate(show);
    print!("{}", runs_table(&runs));
    args.limit.print_summary(show, total, "run");
}

fn runs_table(runs: &[storage::RunSummary]) -> String {
    // Prefix highlighting resolves against every run on disk — the set
    // `view/delete <prefix>` lookups accept — not just the rows shown.
    let peers = match storage::list_runs() {
        Ok(all) => all.into_iter().map(|r| r.run_id).collect(),
        // Listing failed; fall back to the shown rows so prefixes stay
        // correct within the table. Every caller obtained `runs` from
        // the same storage moments ago, so a real storage error has
        // already surfaced through that path.
        Err(_) => runs.iter().map(|r| r.run_id.clone()).collect(),
    };
    let highlighter = IdHighlighter::new(peers);
    let header: Vec<String> = [
        "Run",
        "Started",
        "Tests",
        "Pass",
        "Time · ⌀test",
        "Output",
        "Model",
        "Note",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let rows: Vec<Vec<String>> = runs
        .iter()
        .map(|r| {
            vec![
                highlighter.short(&r.run_id),
                format_elapsed_ms(r.started_at_ms),
                format_tests(r.total),
                format_pass_pct(r.passed, r.total),
                format_run_time(r.duration_ms, r.test_count),
                fmt_tokens_compact(&r.tokens),
                fmt_model(r.model.as_deref()),
                r.note.clone().unwrap_or_default(),
            ]
        })
        .collect();

    let col_count = header.len();
    let mut table = Table::from_iter(std::iter::once(header).chain(rows));
    table
        .with(Style::rounded())
        .modify(Rows::first(), Color::FG_BRIGHT_YELLOW)
        .modify(
            Columns::new(1..col_count - 1).not(Rows::first()),
            style::dim(),
        )
        .modify(Columns::new(4..5), Alignment::right());
    let term_width = console::Term::stdout().size().1 as usize;
    table.with(
        Width::truncate(term_width)
            .suffix("…")
            .priority(OnlyColumn::new(col_count - 1)),
    );
    format!("{table}\n")
}

struct OnlyColumn {
    index: usize,
}

impl OnlyColumn {
    fn new(index: usize) -> Self {
        Self { index }
    }
}

impl Peaker for OnlyColumn {
    fn peak(&mut self, mins: &[usize], widths: &[usize]) -> Option<usize> {
        let w = *widths.get(self.index)?;
        if w == 0 {
            return None;
        }
        if mins.get(self.index).is_some_and(|&m| w <= m) {
            return None;
        }
        Some(self.index)
    }
}

fn fmt_model(model: Option<&str>) -> String {
    model.unwrap_or("").to_string()
}

/// Output token count for the runs table — the cleanest single proxy
/// for work the model did, since input/cached tokens mostly reflect
/// context size rather than effort.
fn fmt_tokens_compact(t: &tokens::Tokens) -> String {
    if t.output == 0 {
        return String::new();
    }
    tokens::format_count(t.output)
}

fn format_tests(total: usize) -> String {
    if total == 0 {
        return "\x1b[3mnone\x1b[23m".to_string();
    }
    total.to_string()
}

fn format_pass_pct(passed: usize, total: usize) -> String {
    if total == 0 {
        return String::new();
    }
    let pct = (passed as f64 / total as f64) * 100.0;
    format!("{pct:.0}%")
}

fn format_run_time(duration_ms: Option<i64>, test_count: usize) -> String {
    let Some(ms) = duration_ms else {
        return "-".to_string();
    };
    let total = format_ms(ms);
    if test_count == 0 {
        return total;
    }
    let per = format_ms(ms / test_count as i64);
    format!("{total} · {per}")
}

fn format_ms(ms: i64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

fn format_elapsed_ms(ms: i64) -> String {
    let now_ms = gage_core::datetime::now_ms();
    let secs = (now_ms - ms) / 1000;
    if secs < 0 {
        "future".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}
