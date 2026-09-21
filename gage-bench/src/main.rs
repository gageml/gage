use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use gage_bench::report::{self, Results};
use gage_bench::session_list_bench::{
    self, BENCH_NAME as SESSION_LIST_BENCH_NAME, Params as SessionListParams,
};
use gage_bench::store_bench::{self, BENCH_NAME, Params};
use indicatif::{ProgressBar, ProgressStyle};

#[derive(Parser, Debug)]
#[command(
    name = "gage-bench",
    about = "Benchmark the Gage store",
    after_help = "Arguments may be read from a file with @FILE, one argument per line. \
Later arguments override earlier ones, so `store @configs/normal.conf --iterations 20` \
runs the normal profile with more iterations.",
    args_override_self = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Populate a fresh store, report sizes, verify, and time reads
    Store(StoreArgs),

    /// Bench `gage session list` against the Claude driver
    ClaudeSessionList(ClaudeSessionListArgs),
}

#[derive(Args, Debug)]
struct ClaudeSessionListArgs {
    /// Total session files to generate
    #[arg(long, default_value_t = 500)]
    sessions: usize,

    /// Number of project slugs the sessions are distributed across
    #[arg(long, default_value_t = 5)]
    projects: usize,

    /// KiB of JSONL content per session file
    #[arg(long, default_value_t = 32)]
    session_kb: usize,

    /// LIMIT value applied to the list queries
    #[arg(long, default_value_t = 20)]
    limit: usize,

    /// Warm-scenario iterations
    #[arg(long, default_value_t = 5)]
    iterations: usize,

    /// Generator seed
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Compare against a saved run: `latest` or a results file path
    #[arg(long, value_name = "FILE|latest")]
    baseline: Option<String>,

    /// Keep the run directory instead of deleting it on success
    #[arg(long)]
    keep: bool,
}

#[derive(Args, Debug)]
struct StoreArgs {
    /// Notes to create
    #[arg(long, default_value_t = 2000)]
    notes: usize,

    /// Bytes per note value
    #[arg(long, default_value_t = 256)]
    note_bytes: usize,

    /// Sessions to add
    #[arg(long, default_value_t = 200)]
    sessions: usize,

    /// KiB of content per session
    #[arg(long, default_value_t = 64)]
    session_kb: usize,

    /// Large sessions to add, reported separately
    #[arg(long, default_value_t = 2)]
    large_sessions: usize,

    /// KiB of content per large session
    #[arg(long, default_value_t = 5120)]
    large_kb: usize,

    /// Datasets to create
    #[arg(long, default_value_t = 10)]
    datasets: usize,

    /// Sessions linked into each dataset
    #[arg(long, default_value_t = 20)]
    dataset_size: usize,

    /// Percent of notes edited and sessions grown after creation
    #[arg(long, default_value_t = 10)]
    edit_pct: u8,

    /// Percent of notes deleted after creation
    #[arg(long, default_value_t = 5)]
    delete_pct: u8,

    /// Repetitions of each read operation
    #[arg(long, default_value_t = 10)]
    iterations: usize,

    /// Reads by id drawn uniformly from the whole population
    #[arg(long, default_value_t = 500)]
    random_reads: usize,

    /// Generator seed
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Compare against a saved run: `latest` or a results file path
    ///
    /// Runs are saved under `gage-bench/results/store/<stamp>.json`.
    #[arg(long, value_name = "FILE|latest")]
    baseline: Option<String>,

    /// Keep the run directory instead of deleting it on success
    #[arg(long)]
    keep: bool,
}

fn main() -> ExitCode {
    // `@path` arguments expand to the file's lines, one argument per
    // line, before clap sees them; typed arguments after the `@path`
    // override the file's. The profiles under `configs/` use this.
    let args = match argfile::expand_args(parse_profile, argfile::PREFIX) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("gage-bench: argument file: {e}");
            return ExitCode::from(2);
        }
    };
    let cli = Cli::parse_from(args);
    match cli.command {
        Command::Store(args) => store(args),
        Command::ClaudeSessionList(args) => claude_session_list(args),
    }
}

fn claude_session_list(args: ClaudeSessionListArgs) -> ExitCode {
    let params = SessionListParams {
        sessions: args.sessions,
        projects: args.projects.max(1),
        session_kb: args.session_kb,
        limit: args.limit.max(1),
        iterations: args.iterations.max(1),
        seed: args.seed,
    };
    let baseline = match args
        .baseline
        .as_deref()
        .map(|s| resolve_baseline_for(SESSION_LIST_BENCH_NAME, s))
    {
        Some(Ok(results)) => Some(results),
        Some(Err(e)) => {
            eprintln!("gage-bench: baseline: {e}");
            return ExitCode::from(2);
        }
        None => None,
    };

    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let run_dir = std::env::temp_dir().join(format!("gage-bench-{stamp}"));
    if let Err(e) = std::fs::create_dir_all(&run_dir) {
        eprintln!("gage-bench: create {}: {e}", run_dir.display());
        return ExitCode::from(1);
    }

    let bar = progress_bar();
    let results = session_list_bench::run(&params, &stamp, &run_dir, &bar);
    bar.finish_and_clear();
    let results = match results {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gage-bench: {e}");
            eprintln!("Run dir: {}", run_dir.display());
            return ExitCode::from(1);
        }
    };

    println!("Run {} {}", results.stamp, results.code_version);
    println!("Params: {}", results.params);
    report::print_metrics("Timings", &results.metrics);
    report::print_sizes("Sizes", &results.sizes);
    report::print_counts("Counts", &results.counts);
    if let Some(baseline) = &baseline {
        report::print_comparison(baseline, &results);
    }

    let saved = match results.save() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("gage-bench: save results: {e}");
            return ExitCode::from(1);
        }
    };
    println!();
    println!("Results: {}", saved.display());
    if args.keep {
        println!("Run dir: {}", run_dir.display());
    } else if let Err(e) = std::fs::remove_dir_all(&run_dir) {
        eprintln!("gage-bench: remove {}: {e}", run_dir.display());
    }
    ExitCode::SUCCESS
}

fn store(args: StoreArgs) -> ExitCode {
    let params = Params {
        notes: args.notes,
        note_bytes: args.note_bytes,
        sessions: args.sessions,
        session_kb: args.session_kb,
        large_sessions: args.large_sessions,
        large_kb: args.large_kb,
        datasets: args.datasets,
        dataset_size: args.dataset_size,
        edit_pct: args.edit_pct.min(100),
        delete_pct: args.delete_pct.min(100),
        iterations: args.iterations.max(1),
        random_reads: args.random_reads,
        seed: args.seed,
    };
    let baseline = match args.baseline.as_deref().map(resolve_baseline) {
        Some(Ok(results)) => Some(results),
        Some(Err(e)) => {
            eprintln!("gage-bench: baseline: {e}");
            return ExitCode::from(2);
        }
        None => None,
    };

    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let run_dir = std::env::temp_dir().join(format!("gage-bench-{stamp}"));
    let home = run_dir.join("home");
    if let Err(e) = std::fs::create_dir_all(&home) {
        eprintln!("gage-bench: create {}: {e}", home.display());
        return ExitCode::from(1);
    }

    let bar = progress_bar();
    let results = store_bench::run(&params, &stamp, &home, &bar);
    bar.finish_and_clear();
    let results = match results {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gage-bench: {e}");
            eprintln!("Run dir: {}", run_dir.display());
            return ExitCode::from(1);
        }
    };

    println!("Run {} {}", results.stamp, results.code_version);
    println!("Params: {}", results.params);
    report::print_metrics("Timings", &results.metrics);
    report::print_sizes("Sizes", &results.sizes);
    report::print_counts("Counts", &results.counts);
    report::print_fsck(&results.fsck);
    if let Some(baseline) = &baseline {
        report::print_comparison(baseline, &results);
    }

    let saved = match results.save() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("gage-bench: save results: {e}");
            return ExitCode::from(1);
        }
    };
    println!();
    println!("Results: {}", saved.display());
    if args.keep {
        println!("Store: {}", home.join("store.git").display());
    } else if let Err(e) = std::fs::remove_dir_all(&home) {
        eprintln!("gage-bench: remove {}: {e}", home.display());
    }
    ExitCode::SUCCESS
}

/// One argument per line, with blank lines and `#` comments dropped.
fn parse_profile(content: &str, prefix: char) -> Vec<argfile::Argument> {
    let kept: String = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    argfile::parse_fromfile(&kept, prefix)
}

/// `latest` selects the newest saved run for this bench; anything else
/// is a results file path.
fn resolve_baseline(spec: &str) -> Result<Results, String> {
    resolve_baseline_for(BENCH_NAME, spec)
}

fn resolve_baseline_for(bench: &str, spec: &str) -> Result<Results, String> {
    let path = if spec == "latest" {
        report::latest_results(bench)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "no saved runs under {}",
                    report::results_dir(bench).display()
                )
            })?
    } else {
        PathBuf::from(spec)
    };
    Results::read(&path).map_err(|e| format!("{}: {e}", path.display()))
}

fn progress_bar() -> ProgressBar {
    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.magenta} {msg:12!} [{elapsed_precise}] \
            {bar:30.white/bright.black} ({pos}/{len})",
        )
        .unwrap()
        .progress_chars("▬▬"),
    );
    bar.enable_steady_tick(Duration::from_millis(120));
    bar
}
