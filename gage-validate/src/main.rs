use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use gage_core::config::default_cache_root;
use gage_validate::Error;
use gage_validate::check::check_source;
use gage_validate::fetch::fetch;
use gage_validate::harness::harness;
use gage_validate::manifest::{self, SourceSpec};
use gage_validate::report::{self, Checked, SourceReport};

#[derive(Parser, Debug)]
#[command(
    name = "gage-validate",
    about = "Validate session drivers against upstream sessions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Download upstream sessions and check each driver against them
    Run(RunArgs),

    /// List upstream sources
    Sources(SourceArgs),
}

#[derive(Args, Debug)]
struct SourceArgs {
    /// Read sources from a file in place of bundled list
    #[arg(long, value_name = "FILE")]
    sources: Option<PathBuf>,

    /// Harness to select (repeatable)
    #[arg(long, value_name = "NAME")]
    harness: Vec<String>,

    /// Source to select (repeatable)
    #[arg(long, value_name = "NAME")]
    source: Vec<String>,
}

#[derive(Args, Debug)]
struct RunArgs {
    #[command(flatten)]
    select: SourceArgs,

    /// Directory to keep downloaded sources in
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let outcome = match cli.command {
        Command::Run(args) => run(args),
        Command::Sources(args) => sources(args),
    };
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("gage-validate: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: RunArgs) -> Result<bool, Error> {
    let cache_dir = args
        .cache_dir
        .unwrap_or_else(|| default_cache_root().join("validate"));
    fs::create_dir_all(&cache_dir)?;
    let mut reports = Vec::new();
    for spec in selected(&args.select)? {
        eprintln!("Checking {}", spec.name);
        reports.push(SourceReport {
            outcome: check(&spec, &cache_dir).map_err(|e| e.to_string()),
            name: spec.name,
            harness: spec.harness,
        });
    }
    print!("{}", report::render(&reports));
    Ok(report::passed(&reports))
}

fn check(spec: &SourceSpec, cache_dir: &std::path::Path) -> Result<Checked, Error> {
    let harness = harness(&spec.harness)
        .ok_or_else(|| Error::Other(format!("no harness named {:?}", spec.harness)))?;
    let checkout = fetch(&spec.git, cache_dir)?;
    let files = spec.files(&checkout.dir)?;
    if files.is_empty() {
        return Err(Error::Other(
            "no file of the source matches its include globs".into(),
        ));
    }
    let work = tempfile::tempdir()?;
    let sessions = check_source(&spec.name, harness, &checkout.dir, &files, work.path())?;
    Ok(Checked {
        revision: checkout.revision,
        cached: checkout.cached,
        sessions,
    })
}

fn sources(args: SourceArgs) -> Result<bool, Error> {
    for spec in selected(&args)? {
        println!(
            "{}\t{}\t{}\t{}",
            spec.name, spec.harness, spec.license, spec.git
        );
    }
    Ok(true)
}

fn selected(args: &SourceArgs) -> Result<Vec<SourceSpec>, Error> {
    let specs = match &args.sources {
        Some(path) => manifest::parse(&fs::read_to_string(path)?)?,
        None => manifest::parse(manifest::BUNDLED)?,
    };
    for name in &args.source {
        if !specs.iter().any(|spec| &spec.name == name) {
            return Err(Error::Other(format!("no source named {name:?}")));
        }
    }
    Ok(specs
        .into_iter()
        .filter(|spec| args.harness.is_empty() || args.harness.contains(&spec.harness))
        .filter(|spec| args.source.is_empty() || args.source.contains(&spec.name))
        .collect())
}
