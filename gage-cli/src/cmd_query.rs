use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::Args;
use datafusion::prelude::SessionContext;
use gage_core::config::default_cache_root;
use gage_publish::Staging;
use gage_query::{ContextBuilder, PrintFormat, ScanScope, exec_command, run_repl};
use gage_store::{ScanStore, Store};

use crate::error::CliError;
use crate::source::SOURCE_HELP;

#[derive(Args)]
pub struct QueryArgs {
    /// Session source to query in place of store
    #[arg(
        long,
        value_name = "SOURCE",
        conflicts_with = "stored_remote",
        long_help = format!(
            "Session source to query in place of store\n\n\
             A source is read and never written. It has session tables \
             alone, as its sessions are whether or not they are stored.\n\n\
             {SOURCE_HELP}"
        )
    )]
    native: Option<String>,

    /// Remote whose fetched objects to query
    ///
    /// Reads objects that gage fetch brought from the remote NAME and
    /// that are not merged into the store. Every table is present.
    /// Nothing is fetched and nothing is written
    #[arg(short = 'R', long, value_name = "NAME")]
    stored_remote: Option<String>,

    /// Query within scan (ID or prefix)
    ///
    /// Scan ID (or prefix). The session tables hold the scan's
    /// sessions at the versions it read, and the scan tables hold
    /// only that scan's tasks, notes, and issues
    #[arg(long, value_name = "SCAN")]
    scan: Option<String>,

    #[command(flatten)]
    run: QueryRunArgs,
}

/// The query options every query command takes: what to run and how
/// to print it. A command adds its own scope options.
#[derive(Args)]
pub struct QueryRunArgs {
    /// SQL to execute, or a .sql file to run
    ///
    /// May be given multiple times; statements run in order. A value
    /// naming an existing file is read as SQL; its statements end with
    /// `;` and `--` lines are comments
    sql: Vec<String>,

    /// Output format (default table)
    ///
    /// One of table, csv, json, ndjson, or yaml
    #[arg(
        short,
        long,
        default_value = "table",
        hide_default_value = true,
        hide_possible_values = true
    )]
    format: PrintFormat,

    /// Suppress non-result output
    #[arg(short, long)]
    quiet: bool,

    /// Enable query timings
    #[arg(long)]
    timing: bool,

    /// Enable query stats
    #[arg(long)]
    stats: bool,

    /// Include system columns
    ///
    /// Columns Gage uses to display and address rows, such as
    /// id_prefix, locator, and commit, are hidden by default. This
    /// option includes them.
    #[arg(long)]
    system: bool,
}

pub async fn main(args: QueryArgs) -> Result<(), CliError> {
    if let Some(spec) = args.native.as_deref() {
        if args.scan.is_some() {
            return Err(CliError::message(format!(
                "gage query: --scan reads a store, and --native {spec} is a session directory"
            )));
        }
        let ctx = builder(None, &args.run)
            .build_source(spec)
            .with_context(|| format!("gage query: --native {spec}"))?;
        return run("gage query", &ctx, &args.run).await;
    }
    let store = match args.stored_remote.as_deref() {
        None => Store::open(&gage_store::store_path()).context("gage query")?,
        Some(remote) => {
            Store::open_stored_remote(&gage_store::store_path(), &default_cache_root(), remote)
                .with_context(|| format!("gage query: --stored-remote {remote}"))?
        }
    };
    let scope = match &args.scan {
        Some(prefix) => {
            let record = ScanStore::from(&store)
                .get(prefix)
                .with_context(|| format!("gage query: --scan {prefix}"))?;
            Some(ScanScope::stored(record.id))
        }
        None => None,
    };
    let mut builder = builder(Some(store), &args.run);
    if let Some(scope) = scope {
        builder = builder.scope(scope);
    }
    run("gage query", &builder.build().await, &args.run).await
}

/// Query the pending publish sessions. The context holds the session
/// tables over the pending set and nothing else.
pub async fn publish(args: QueryRunArgs) -> Result<(), CliError> {
    let store = Store::open(&gage_store::store_path()).context("gage publish query")?;
    let sessions = Staging::from(&store)
        .external()
        .context("gage publish query")?;
    let builder = builder(Some(store), &args).external(sessions);
    run("gage publish query", &builder.build().await, &args).await
}

/// A context builder over `store`, with the system columns hidden
/// unless `args` asks for them.
fn builder(store: Option<Store>, args: &QueryRunArgs) -> ContextBuilder {
    let builder = ContextBuilder::new(store.map(|store| Arc::new(Mutex::new(store))));
    if args.system {
        builder
    } else {
        builder.skip_system_cols()
    }
}

/// Run the statements on `ctx`, or the REPL when there are none.
async fn run(command: &str, ctx: &SessionContext, args: &QueryRunArgs) -> Result<(), CliError> {
    let result = if args.sql.is_empty() {
        run_repl(
            ctx,
            gage_query::repl_functions(),
            args.format,
            args.quiet,
            args.timing,
            args.stats,
        )
        .await
    } else {
        let statements = statements(&args.sql).context(command.to_string())?;
        let mut result = Ok(());
        for sql in &statements {
            result = exec_command(ctx, sql, args.format).await;
            if result.is_err() {
                break;
            }
        }
        result
    };
    result.map_err(|e| CliError::message(format!("Error: {e}")))
}

/// The statements the positional arguments name, in order: an argument
/// that is an existing file is read and split, any other is one
/// statement as given.
fn statements(args: &[String]) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    for arg in args {
        let path = Path::new(arg);
        if path.is_file() {
            let text = fs::read_to_string(path)
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
            out.extend(split_statements(&text));
        } else {
            out.push(arg.clone());
        }
    }
    Ok(out)
}

/// Split a SQL file into statements: `--` lines are dropped, and `;`
/// ends a statement. Empty statements are skipped.
fn split_statements(text: &str) -> Vec<String> {
    let code: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect();
    code.join("\n")
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_splits_on_semicolons_and_drops_comment_lines() {
        let text = "-- recent sessions\nSELECT 1;\n\n-- two\nSELECT 2\n  FROM t;\n";
        assert_eq!(split_statements(text), ["SELECT 1", "SELECT 2\n  FROM t"]);
    }

    #[test]
    fn a_non_file_argument_is_one_statement() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("q.sql");
        fs::write(&file, "SELECT 1; SELECT 2;").unwrap();
        let args = [file.to_string_lossy().into_owned(), "SELECT 3".into()];
        assert_eq!(
            statements(&args).unwrap(),
            ["SELECT 1", "SELECT 2", "SELECT 3"]
        );
    }
}
