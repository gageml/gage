use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use clap::Args;
use gage_query::PrintFormat;
use gage_query2::{ContextBuilder, ScanScope};
use gage_store::{ScanStore, Store};

#[derive(Args)]
pub struct Query2Args {
    /// SQL to execute, or a .sql file to run
    ///
    /// May be given multiple times; statements run in order. A value
    /// naming an existing file is read as SQL; its statements end with
    /// `;` and `--` lines are comments
    sql: Vec<String>,

    /// Output format
    #[arg(short, long, default_value = "table")]
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
    /// Session tables hide `id_display`, `id_prefix`, and the row
    /// locator by default. This option adds them to the schema.
    #[arg(long)]
    system: bool,

    /// Query within a scan
    ///
    /// Scan ID (or prefix). The session tables hold the scan's
    /// sessions at the versions it read, and the scan tables hold
    /// only that scan's tasks, notes, and issues
    #[arg(long, value_name = "SCAN")]
    scan: Option<String>,
}

pub async fn main(args: Query2Args) {
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage query2: {e}");
            std::process::exit(1);
        }
    };
    let scope = match &args.scan {
        Some(prefix) => match ScanStore::from(&store).get(prefix) {
            Ok(record) => Some(ScanScope::stored(record.id)),
            Err(e) => {
                eprintln!("gage query2: --scan {prefix}: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };
    let mut builder = ContextBuilder::new(Some(Arc::new(Mutex::new(store))));
    if let Some(scope) = scope {
        builder = builder.scope(scope);
    }
    if !args.system {
        builder = builder.skip_system_cols();
    }
    let ctx = builder.build().await;
    let result = if args.sql.is_empty() {
        gage_query::run_repl(
            &ctx,
            None,
            gage_query2::repl_functions(),
            args.format,
            args.quiet,
            args.timing,
            args.stats,
        )
        .await
    } else {
        let statements = match statements(&args.sql) {
            Ok(statements) => statements,
            Err(e) => {
                eprintln!("gage query2: {e}");
                std::process::exit(1);
            }
        };
        let mut result = Ok(());
        for sql in &statements {
            result = gage_query::exec_command(&ctx, sql, args.format).await;
            if result.is_err() {
                break;
            }
        }
        result
    };
    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
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
