//! `gage dev`: commands for developing Gage itself, present in debug
//! builds only. A release build has no `dev` command.

use std::thread;
use std::time::Duration;

use clap::{Args, Subcommand};
use gage_session::progress;
use tracing::Span;

use crate::error::CliError;
use crate::progress::busy;

#[derive(Subcommand)]
pub enum DevCommand {
    /// Exercise progress indicator with synthetic work
    TestProgress(TestProgressArgs),
}

pub fn run(command: DevCommand) -> Result<(), CliError> {
    match command {
        DevCommand::TestProgress(args) => test_progress(args),
    }
}

#[derive(Args)]
pub struct TestProgressArgs {
    /// Steps of work to report (default 20)
    #[arg(short = 'n', long, default_value_t = 20, hide_default_value = true)]
    total: usize,

    /// Seconds work takes (default 2)
    #[arg(short, long, default_value_t = 2.0, hide_default_value = true)]
    seconds: f64,

    /// Work to report (default read_sessions)
    ///
    /// Work to report. One of read_sessions or index_text
    #[arg(
        short,
        long,
        default_value = "read_sessions",
        hide_default_value = true
    )]
    work: String,

    /// Concurrent reports of work (default 1)
    #[arg(short, long, default_value_t = 1, hide_default_value = true)]
    parallel: usize,

    /// Seconds of busy row before work begins (default 0)
    #[arg(short, long, default_value_t = 0.0, hide_default_value = true)]
    busy: f64,

    /// Log a warning halfway through work
    #[arg(long)]
    warn: bool,
}

fn test_progress(args: TestProgressArgs) -> Result<(), CliError> {
    let begin: fn(usize) -> Span = match args.work.as_str() {
        progress::READ_SESSIONS => progress::read_sessions,
        progress::INDEX_TEXT => progress::index_text,
        other => {
            return Err(CliError::message(format!(
                "gage dev test-progress: unknown work {other:?}; use {} or {}",
                progress::READ_SESSIONS,
                progress::INDEX_TEXT
            )));
        }
    };
    if args.busy > 0.0 {
        busy("Preparing", || {
            thread::sleep(Duration::from_secs_f64(args.busy))
        });
    }
    let total = args.total;
    let step = Duration::from_secs_f64(args.seconds / total.max(1) as f64);
    let workers = args.parallel.max(1);
    busy("Working", || {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let warn = args.warn;
                thread::spawn(move || {
                    let span = begin(total);
                    for done in 1..=total {
                        thread::sleep(step);
                        progress::advance(&span, done);
                        if warn && done == total / 2 {
                            tracing::warn!("test-progress: halfway through {total} steps");
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });
    println!(
        "Reported {total} steps of {} over {}s in {workers} worker(s)",
        args.work, args.seconds
    );
    Ok(())
}
