use std::io::{self, Write};
use std::path::PathBuf;

use clap::Args;
use gage_registry::scanner::parse_scanner_file;
use gage_runtime2::Output;
use gage_scan2::{CompiledScanner, Event};

#[derive(Args)]
pub struct Scan2Args {
    /// Scanner file to run (repeatable)
    #[arg(short, long = "file", value_name = "PATH", required = true)]
    files: Vec<PathBuf>,
}

pub async fn main(args: Scan2Args) {
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

    let summary = gage_scan2::run(&scanners, |event| match event {
        Event::Output(Output::Print(s)) => print!("{s}"),
        Event::Output(Output::Println(s)) => println!("{s}"),
        Event::TaskFailed {
            scanner,
            task,
            message,
        } => {
            eprintln!("gage scan2: {scanner}:{task}: {message}");
        }
    })
    .await;
    io::stdout().flush().unwrap();

    if summary.failed > 0 {
        std::process::exit(1);
    }
}
