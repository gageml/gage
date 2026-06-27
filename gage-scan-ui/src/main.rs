//! Mock harness for experimenting with `gage scan` progress UIs.
//!
//! Runs a real scan and routes the scanner runner's events to one of
//! several alternative UI prototypes selected by MODE.

use std::str::FromStr;

use clap::Parser;

mod scan;
mod sink;
mod ui_indicatif;
mod ui_ratatui;

#[derive(Copy, Clone, Debug)]
enum Mode {
    Indicatif,
    RatatuiDense,
    RatatuiTwoLine,
    RatatuiFull,
}

impl FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "indicatif" => Ok(Mode::Indicatif),
            "dense" | "ratatui-dense" => Ok(Mode::RatatuiDense),
            "two-line" | "ratatui-two-line" => Ok(Mode::RatatuiTwoLine),
            "full" | "ratatui-full" => Ok(Mode::RatatuiFull),
            other => Err(format!(
                "unknown mode '{other}' (expected: indicatif | dense | two-line | full)"
            )),
        }
    }
}

#[derive(Parser)]
#[command(
    name = "gage-scan-ui",
    about = "Prototype harness for gage scan progress UIs",
    disable_help_subcommand = true
)]
struct Cli {
    /// UI mode: indicatif | dense | two-line | full
    mode: Mode,

    /// Session IDs to scan (or prefix)
    #[arg(value_name = "SESSION")]
    sessions: Vec<String>,

    /// Scanner to run (repeatable)
    #[arg(short, long = "scanner", value_name = "NAME")]
    scanners: Vec<String>,

    /// Scanner file to run (repeatable)
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    files: Vec<String>,

    /// Scan most recent N sessions
    #[arg(short, long, value_name = "N", conflicts_with = "sessions")]
    limit: Option<usize>,

    /// Maximum concurrent task workers
    #[arg(short, long, value_name = "N")]
    jobs: Option<usize>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let mode = cli.mode;
    let setup = match scan::Setup::resolve(scan::SetupArgs {
        sessions: cli.sessions,
        scanners: cli.scanners,
        files: cli.files,
        limit: cli.limit,
        jobs: cli.jobs,
    })
    .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let result = match mode {
        Mode::Indicatif => ui_indicatif::run(setup).await,
        Mode::RatatuiDense => ui_ratatui::run(setup, ui_ratatui::Layout::Dense).await,
        Mode::RatatuiTwoLine => ui_ratatui::run(setup, ui_ratatui::Layout::TwoLine).await,
        Mode::RatatuiFull => ui_ratatui::run(setup, ui_ratatui::Layout::Full).await,
    };

    match result {
        Ok(summary) => {
            println!(
                "{} tasks ({} failed, {} skipped) in {:.1}s — scan {}",
                summary.completed,
                summary.failed,
                summary.skipped,
                summary.elapsed_secs,
                &summary.scan_id[..8.min(summary.scan_id.len())],
            );
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
