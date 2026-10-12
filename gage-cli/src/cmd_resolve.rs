//! `gage resolve` — resolve issues in an interactive session the
//! default driver starts in its harness.

use clap::Args;
use cliclack as cli;
use console::{Term, style};
use gage_store::{IssueStore, Store};

use crate::dialog::{self, DialogError, DialogResult};
use crate::model_prompt;
use crate::source;

#[derive(Args)]
pub struct ResolveArgs {
    /// Issues to resolve (ID or prefix)
    ///
    /// If omitted, all pending and open issues are in scope.
    ids: Vec<String>,

    /// Model for session
    ///
    /// If omitted, the dialog prompts for a model.
    #[arg(short, long)]
    model: Option<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,

    /// Additional arguments passed through to harness
    #[arg(last = true, value_name = "ARGS")]
    extra_args: Vec<String>,
}

pub fn run(args: ResolveArgs) {
    dialog::run("Resolve issues", || run_dialog(&args));
}

fn run_dialog(args: &ResolveArgs) -> Result<DialogResult, DialogError> {
    // Every argument resolves to a full id before the session starts,
    // so the skill never sees a prefix
    let mut ids: Vec<String> = Vec::new();
    if !args.ids.is_empty() {
        let store = open_store("gage resolve");
        let issues = IssueStore::from(&store);
        let mut errors = 0;
        for prefix in &args.ids {
            match issues.get(prefix) {
                Ok(issue) => ids.push(issue.id),
                Err(e) => {
                    cli::log::error(format!("{prefix}: {e}"))?;
                    errors += 1;
                }
            }
        }
        if errors > 0 {
            return Err(DialogError::Failed("Unknown issues".to_string()));
        }
    }

    cli::log::info(dialog::wrap_text(
        &Term::stdout(),
        "You are about to start an interactive session to resolve issues. \
         Standard token usage applies for the selected model.",
    ))?;
    cli::log::step(format!("Issues\n{}", style(issues_label(&ids)).dim()))?;

    let model = match &args.model {
        Some(m) => {
            cli::log::step(format!("Model\n{}", style(m).dim()))?;
            Some(m.clone())
        }
        None => model_prompt::prompt_model_ranked()?,
    };

    if !args.yes {
        let confirmed = cli::confirm("Start session?")
            .initial_value(true)
            .interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    let driver = source::default_driver_or_exit("gage resolve");
    let mut session = driver
        .start_resolve_session(&ids, model.as_deref(), &args.extra_args)
        .map_err(|e| DialogError::Failed(e.to_string()))?;
    let code = session
        .wait()
        .map_err(|e| DialogError::Other(anyhow::anyhow!("waiting for the session: {e}")))?;
    if code != 0 {
        return Err(DialogError::Failed(format!(
            "resolve session exited with status {code}"
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

fn open_store(command: &str) -> Store {
    match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    }
}
