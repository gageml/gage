use clap::Args;
use gage_query::PrintFormat;

#[derive(Args)]
pub struct QueryArgs {
    /// Operate on agent sessions instead of Claude Code sessions
    #[arg(short = 'A', long)]
    pub agent: bool,

    /// Execute SQL and exit
    #[arg(short, long)]
    command: Option<String>,

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
    let ctx = gage_query::create_context_default().await;
    let result = if let Some(sql) = args.command {
        gage_query::exec_command(&ctx, &sql, args.format).await
    } else {
        gage_query::run_repl(
            &ctx,
            Some(gage_query::default_index_store()),
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
