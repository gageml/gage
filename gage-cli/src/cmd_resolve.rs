//! `gage resolve` — resolve issues in an interactive Claude Code session.

use clap::Args;
use cliclack as cli;
use console::style;
use gage_claude::model::{configured_model, resolve_model};
use gage_claude::resolve::resolve_command;
use gage_db::db;
use gage_db::issue;

use crate::dialog::{self, DialogError, DialogResult};
use crate::model_prompt::{self, DefaultModel};

#[derive(Args)]
pub struct ResolveArgs {
    /// Issue IDs (or prefixes) to scope the session
    ///
    /// If omitted, all pending and open issues are in scope.
    ids: Vec<String>,

    /// Model for the session
    ///
    /// If omitted, the dialog prompts for a model.
    #[arg(long)]
    model: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    /// Additional arguments passed to claude
    #[arg(last = true, value_name = "CLAUDE_ARGS")]
    claude_args: Vec<String>,
}

pub fn run(args: ResolveArgs) {
    dialog::run("Resolve issues", || run_dialog(&args));
}

fn run_dialog(args: &ResolveArgs) -> Result<DialogResult, DialogError> {
    let conn = db::open_db().unwrap();
    let mut ids: Vec<String> = Vec::new();
    let mut errors = 0;
    for prefix in &args.ids {
        match issue::get(&conn, prefix) {
            Ok(i) => ids.push(i.id),
            Err(e) => {
                cli::log::error(e.to_string())?;
                errors += 1;
            }
        }
    }
    if errors > 0 {
        return Err(DialogError::Failed("Unknown issues".to_string()));
    }

    cli::log::info(dialog::wrap_text(
        "You are about to start an interactive Claude Code session to resolve issues. \
         Standard token usage applies for the selected model.",
    ))?;
    cli::log::step(format!("Issues\n{}", style(issues_label(&ids)).dim()))?;

    let model = match &args.model {
        Some(m) => {
            let model = resolve_model(m).to_string();
            cli::log::step(format!("Model\n{}", style(&model).dim()))?;
            model
        }
        None => prompt_model()?,
    };

    if !args.yes {
        let confirmed = cli::confirm("Start session?")
            .initial_value(true)
            .interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    let status = resolve_command(&ids, &model, &args.claude_args)?
        .status()
        .map_err(|e| DialogError::Other(anyhow::anyhow!("failed to run claude: {e}")))?;
    if !status.success() {
        return Err(DialogError::Failed(format!(
            "claude exited with status {}",
            status.code().unwrap_or(1)
        )));
    }
    Ok(DialogResult::from("Resolve session ended"))
}

fn issues_label(ids: &[String]) -> String {
    if ids.is_empty() {
        "all pending and open".to_string()
    } else {
        ids.join("\n")
    }
}

fn prompt_model() -> Result<String, DialogError> {
    let configured = std::env::current_dir()
        .ok()
        .and_then(|cwd| configured_model(&cwd));
    let default = configured.map(|m| DefaultModel {
        model: m,
        note: "from settings",
    });
    model_prompt::prompt_model(default)
}
