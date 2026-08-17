//! `gage review` — review issues in an interactive Claude Code session.

use clap::Args;
use gage_db::db;
use gage_db::issue;

#[derive(Args)]
pub struct ReviewArgs {
    /// Issue IDs (or prefixes) to scope the review
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

pub fn run(args: ReviewArgs) {
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

    let claude_bin = match gage_claude::proc::find_claude() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let mut prompt = String::from("/gage:review");
    for id in &ids {
        prompt.push(' ');
        prompt.push_str(id);
    }

    let mut cmd = std::process::Command::new(&claude_bin);
    cmd.arg("-n").arg("Review Gage issues");
    if let Some(model) = &args.model {
        cmd.arg("--model").arg(model);
    }
    cmd.args(&args.claude_args);
    cmd.arg(prompt);

    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("Error: failed to run claude: {e}");
            std::process::exit(1);
        }
    }
}
