//! `gage resolve` — resolve issues in an interactive Claude Code session.

use clap::Args;
use gage_claude::resolve::resolve_command;
use gage_db::db;
use gage_db::issue;

#[derive(Args)]
pub struct ResolveArgs {
    /// Issue IDs (or prefixes) to scope the session
    ///
    /// If omitted, all pending and open issues are in scope.
    ids: Vec<String>,

    /// Model for the session
    #[arg(long)]
    model: Option<String>,

    /// Additional arguments passed to claude
    #[arg(last = true, value_name = "CLAUDE_ARGS")]
    claude_args: Vec<String>,
}

pub fn run(args: ResolveArgs) {
    let conn = db::open_db().unwrap();
    let mut ids: Vec<String> = Vec::new();
    let mut errors = 0;
    for prefix in &args.ids {
        match issue::get(&conn, prefix) {
            Ok(i) => ids.push(i.id),
            Err(e) => {
                eprintln!("{e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let mut cmd = match resolve_command(&ids, args.model.as_deref(), &args.claude_args) {
        Ok(cmd) => cmd,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("Error: failed to run claude: {e}");
            std::process::exit(1);
        }
    }
}
