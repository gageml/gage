use clap::{Args, Parser, Subcommand};
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

mod eval;
mod limit;
mod run;
mod score;
mod storage;
mod style;
mod tokens;
mod view;

#[derive(Parser)]
#[command(name = "gage-eval", about = "Run Gage MCP evals")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
struct ListArgs {
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
struct DeleteArgs {
    /// Run UUIDs or unique prefixes. Each must match exactly one run
    run_ids: Vec<String>,

    /// Skip the confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
struct ViewArgs {
    /// Run UUID or unique prefix
    run_id: String,

    /// Rebuild report.md even if it already exists
    #[arg(long)]
    refresh: bool,
}

const DEFAULT_MODEL: &str = "sonnet";
const DEFAULT_EFFORT: &str = "low";

#[derive(Args)]
struct RunArgs {
    /// Tests to run (default: all)
    ///
    /// `*` matches everything; `eval/test` matches one test; a bare
    /// token matches that test-id in any eval, or every test in an eval
    /// of that name. `*` does not cross `/`. Prefix any spec with `!` to
    /// exclude.
    specs: Vec<String>,

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
    /// Stored in manifest.json and shown in `gage-eval list`. Useful for
    /// labeling what you were varying.
    #[arg(short, long)]
    note: Option<String>,

    /// Run without being prompted
    #[arg(short, long)]
    yes: bool,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => cmd_run(args),
        Command::List(args) => cmd_list(args),
        Command::View(args) => cmd_view(args),
        Command::Delete(args) => cmd_delete(args),
    }
}

fn cmd_view(args: ViewArgs) {
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
    resolved.sort_by(|a, b| b.started_at_ms.cmp(&a.started_at_ms));

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
    let all = match eval::load_all() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to load evals: {e}");
            std::process::exit(2);
        }
    };
    let tests = match eval::select(&all, &args.specs) {
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

    if let Err(missing) = eval::validate(&tests) {
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

    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("ANTHROPIC_API_KEY is required");
        std::process::exit(1);
    }

    if let Err(e) = show_run_intro(&tests, &args.model, &args.effort) {
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
    let result = match run::run_batch(
        &tests,
        &args.model,
        &args.effort,
        args.note.as_deref(),
        |evt| match evt {
            run::Event::Started(name) => pb.set_message(name.to_string()),
            run::Event::TestFinished {
                name,
                exit_code,
                score,
            } => {
                pb.inc(1);
                let bar = console::style("│").bright().black();
                if exit_code != 0 {
                    error_count += 1;
                    let msg = console::style(format!("  {name}  exit={exit_code}")).red();
                    pb.println(format!("{bar} {msg}"));
                }
                if let Some(s) = score {
                    if s.passed {
                        passed += 1;
                        pb.println(format!("{bar}   ✓ {name}"));
                    } else {
                        failed += 1;
                        let missed: Vec<&str> = s
                            .matches
                            .iter()
                            .filter(|m| !m.matched)
                            .map(|m| m.pattern.as_str())
                            .collect();
                        let msg = console::style(format!("✗ {name}  missed: {missed:?}")).red();
                        pb.println(format!("{bar}   {msg}"));
                    }
                }
            }
        },
    ) {
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
    if error_count > 0 {
        cliclack::outro_cancel(format!(
            "Run {} completed with errors (see above for details) in {elapsed}",
            result.run_id
        ))
        .unwrap();
    } else {
        cliclack::outro(
            console::style(format!("Run {} completed in {elapsed}", result.run_id))
                .green()
                .bright(),
        )
        .unwrap();
    }
}

fn show_run_intro(tests: &[&eval::Test], model: &str, effort: &str) -> std::io::Result<()> {
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

    let model_suffix = if model == DEFAULT_MODEL {
        " (default)"
    } else {
        ""
    };
    cliclack::log::remark(format!("Model: {model}{model_suffix}"))?;

    let effort_suffix = if effort == DEFAULT_EFFORT {
        " (default)"
    } else {
        ""
    };
    cliclack::log::remark(format!("Effort: {effort}{effort_suffix}"))?;
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
                short_uuid(&r.run_id).to_string(),
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
        .modify(Columns::first().not(Rows::first()), Color::FG_BRIGHT_YELLOW)
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
