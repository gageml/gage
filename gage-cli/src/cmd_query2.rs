use std::sync::{Arc, Mutex};

use clap::Args;
use gage_query::PrintFormat;
use gage_query2::SessionBacking;
use gage_store::Store;

#[derive(Args)]
pub struct Query2Args {
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

pub async fn main(args: Query2Args) {
    let store = match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("gage query2: {e}");
            std::process::exit(1);
        }
    };
    let ctx = match gage_query2::context(SessionBacking::Store(Arc::new(Mutex::new(store)))) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("gage query2: {e}");
            std::process::exit(1);
        }
    };
    let result = if args.sql.is_empty() {
        gage_query::run_repl(&ctx, None, args.format, args.quiet, args.timing, args.stats).await
    } else {
        let mut result = Ok(());
        for sql in &args.sql {
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
