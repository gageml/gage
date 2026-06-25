use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use chrono::Utc;
use clap::Parser;
use gage_bench::benches::{self};
use gage_bench::corpus::{self, LoadOptions};
use gage_bench::formats;
use gage_bench::report::{self, RowData};
use indicatif::{ProgressBar, ProgressStyle};

const DEFAULT_SESSION_LIMIT: usize = 100;
const DEFAULT_ITERATIONS: usize = 10;

#[derive(Parser, Debug)]
#[command(
    name = "gage-bench",
    about = "Benchmark serialization formats against real gage session data"
)]
struct Cli {
    /// Names of benchmarks to run
    ///
    /// Use --list to list available benchmarks.
    names: Vec<String>,

    /// Maximum number of sessions to load (default 100)
    #[arg(short, long)]
    limit: Option<usize>,

    /// Load all sessions
    #[arg(short, long)]
    all: bool,

    /// Iterations per format (default 10)
    #[arg(short, long)]
    iterations: Option<usize>,

    /// List available benchmark names
    #[arg(long)]
    list: bool,

    /// Override the source root (default ~/.claude/projects)
    #[arg(long)]
    root: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.list {
        for b in benches::registry() {
            println!("{:10}  {}", b.name, b.description);
        }
        return ExitCode::SUCCESS;
    }

    if cli.names.is_empty() {
        eprintln!("Error: at least one benchmark name required (try --list)");
        return ExitCode::from(2);
    }

    let mut selected: Vec<&'static benches::Bench> = Vec::new();
    for name in &cli.names {
        match benches::find(name) {
            Some(b) => selected.push(b),
            None => {
                eprintln!("Error: unknown benchmark: {name}");
                return ExitCode::from(2);
            }
        }
    }

    let formats = formats::compiled();
    if formats.is_empty() {
        eprintln!("Error: no formats compiled in — enable at least one fmt-* feature");
        return ExitCode::from(2);
    }

    let limit = if cli.all {
        None
    } else {
        Some(cli.limit.unwrap_or(DEFAULT_SESSION_LIMIT))
    };

    let sessions = match corpus::pick_sessions(&LoadOptions {
        root: cli.root.clone(),
        limit,
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::from(1);
        }
    };

    let load_bar = make_progress_bar(0);
    load_bar.set_message("Loading sessions");
    let corpus = match corpus::load(sessions, &load_bar) {
        Ok(c) => {
            load_bar.finish_and_clear();
            c
        }
        Err(e) => {
            load_bar.finish_and_clear();
            eprintln!("Error: {e}");
            return ExitCode::from(1);
        }
    };
    println!(
        "Loaded {} sessions, {} total entries",
        corpus.session_count(),
        corpus.total_entries()
    );

    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let outdir = PathBuf::from(format!("/tmp/gage-bench-{stamp}"));
    std::fs::create_dir_all(&outdir).unwrap();

    let iterations = cli.iterations.unwrap_or(DEFAULT_ITERATIONS).max(1);
    let mut all_rows: Vec<RowData> = Vec::new();
    for b in &selected {
        println!();
        println!("== Bench: {} ==", b.name);
        let progress_len = (formats.len() * corpus.session_count() * 2 * iterations) as u64;
        let bar = make_progress_bar(progress_len);
        let rows = (b.run)(&corpus, &formats, &outdir, iterations, &bar);
        bar.finish_and_clear();
        report::print_table(&rows);
        all_rows.extend(rows);
    }

    let json_path = outdir.join("results.json");
    if let Err(e) = report::write_json(&json_path, &all_rows) {
        eprintln!("Warn: results.json write failed: {e}");
    }

    println!();
    println!("Bench dir: {}", outdir.display());

    ExitCode::SUCCESS
}

fn make_progress_bar(len: u64) -> ProgressBar {
    let bar = ProgressBar::new(len);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.magenta} {msg:16!} [{elapsed_precise}] \
            {bar:30.white/bright.black} ({pos}/{len})",
        )
        .unwrap()
        .progress_chars("▬▬"),
    );
    bar.enable_steady_tick(Duration::from_millis(120));
    bar
}
