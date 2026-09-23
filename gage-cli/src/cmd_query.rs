use clap::Args;
use gage_query::PrintFormat;

use crate::source;

#[derive(Args)]
pub struct QueryArgs {
    /// Session source
    ///
    /// A driver scheme selects the driver (`claude:<path>`); a value
    /// with no scheme goes to the default driver (`<path>`). Defaults
    /// to the default driver's default location.
    #[arg(short, long, value_name = "SOURCE")]
    source: Option<String>,

    /// Execute SQL and exit
    ///
    /// May be given multiple times; statements run in order.
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
}

pub async fn main(args: QueryArgs) {
    let source = source::open_source_or_exit("gage query", args.source.as_deref().unwrap_or(""));
    let ctx = match gage_query::create_context(source.as_ref()).await {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("gage query: {e}");
            std::process::exit(1);
        }
    };
    let result = if !args.sql.is_empty() {
        let mut result = Ok(());
        for sql in &args.sql {
            result = gage_query::exec_command(&ctx, sql, args.format).await;
            if result.is_err() {
                break;
            }
        }
        result
    } else {
        gage_query::run_repl(
            &ctx,
            Some(source::index_store_or_exit("gage query", source.as_ref())),
            gage_query::tables::registered_tvfs(),
            args.format,
            args.quiet,
            args.timing,
            args.stats,
        )
        .await
    };
    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
