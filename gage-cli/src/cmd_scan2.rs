use std::io::{self, Write};
use std::path::PathBuf;

use clap::Args;
use gage_registry::scanner::parse_scanner_file;
use gage_runtime2::Output;
use gage_scan2::staging::staging_root;
use gage_scan2::{CompiledScanner, Event, ScanOutcome};
use gage_store::{Store, TaskStatus};

#[derive(Args)]
pub struct Scan2Args {
    /// Scanner file to run (repeatable)
    #[arg(short, long = "file", value_name = "PATH", required = true)]
    files: Vec<PathBuf>,
}

pub async fn main(args: Scan2Args) {
    // The store is needed only at the end, but a missing store is a
    // full stop before any work.
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage scan2: {e}");
            std::process::exit(1);
        }
    };

    let mut defs = Vec::new();
    let mut errors = 0;
    for path in &args.files {
        match parse_scanner_file(path) {
            Ok(def) => defs.push(def),
            Err(e) => {
                eprintln!("gage scan2: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    // Every scanner compiles before any task runs, so a broken
    // scanner is a full stop.
    let mut scanners: Vec<CompiledScanner> = Vec::new();
    for def in &defs {
        match gage_scan2::compile(def) {
            Ok(s) => scanners.push(s),
            Err(e) => {
                eprintln!("gage scan2: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    // Ctrl-C cancels the run; the scan applies what ran. Once tokio
    // has taken the signal, a second Ctrl-C during apply has no
    // effect.
    let cancel = crate::panic_token().child_token();
    let signal_task = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("gage scan2: canceling");
                    cancel.cancel();
                }
                _ = cancel.cancelled() => {}
            }
        })
    };

    let result = gage_scan2::scan(
        &store,
        &staging_root(),
        &scanners,
        &cancel,
        |event| match event {
            Event::Output(Output::Print(s)) => print!("{s}"),
            Event::Output(Output::Println(s)) => println!("{s}"),
            Event::TaskStarted { .. } => {}
            Event::TaskFinished {
                scanner,
                task,
                status: TaskStatus::Failed,
                error,
            } => {
                let message = error.unwrap_or_default();
                eprintln!("gage scan2: {scanner}:{task}: {message}");
            }
            Event::TaskFinished { .. } => {}
        },
    )
    .await;
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
    eprintln!("{}", summary_line(&outcome));
    let attrs = &outcome.attrs;
    if attrs.canceled || attrs.tasks.failed > 0 {
        std::process::exit(1);
    }
}

fn summary_line(outcome: &ScanOutcome) -> String {
    let attrs = &outcome.attrs;
    let state = if attrs.canceled {
        "canceled"
    } else {
        "completed"
    };
    let counts = &attrs.tasks;
    let mut parts = vec![
        format!("{} completed", counts.completed),
        format!("{} failed", counts.failed),
    ];
    let canceled = counts.total - counts.completed - counts.failed - counts.skipped;
    if canceled > 0 {
        parts.push(format!("{canceled} canceled"));
    }
    format!(
        "scan {} {state}: {} tasks: {}",
        outcome.id,
        counts.total,
        parts.join(", ")
    )
}
