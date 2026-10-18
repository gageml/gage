//! `gage resolve` — resolve issues in an interactive session the
//! default driver starts in its harness.

use anyhow::Context;
use clap::Args;
use cliclack as cli;
use console::{Term, style};
use gage_core::uuid::short_uuid;
use gage_store::{IssueFull, IssueStore, Store};

use crate::dialog::{self, DialogError, DialogResult};
use crate::error::CliError;
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

    /// Skip prompts
    ///
    /// Without --model, model takes its default
    #[arg(short, long)]
    yes: bool,

    /// Additional arguments passed through to harness
    #[arg(last = true, value_name = "ARGS")]
    extra_args: Vec<String>,
}

pub fn run(args: ResolveArgs) -> Result<(), CliError> {
    // The dialog settles the issues and the model; the session runs
    // after it, since the harness owns the terminal from then on
    let mut session: Option<ResolveSetup> = None;
    dialog::run("Resolve issues", || run_dialog(&args, &mut session))?;
    let Some(setup) = session else {
        return Ok(());
    };
    let driver = source::default_driver("gage resolve")?;
    let mut session = driver
        .start_resolve_session(&setup.ids, setup.model.as_deref(), &args.extra_args)
        .context("gage resolve")?;
    let code = session
        .wait()
        .context("gage resolve: waiting for the session")?;
    if code != 0 {
        return Err(CliError::message(format!(
            "gage resolve: session exited with status {code}"
        )));
    }
    Ok(())
}

/// What the dialog settled for the session
struct ResolveSetup {
    ids: Vec<String>,
    model: Option<String>,
}

fn run_dialog(
    args: &ResolveArgs,
    setup: &mut Option<ResolveSetup>,
) -> Result<DialogResult, DialogError> {
    // Every argument resolves to a full id before the session starts,
    // so the skill never sees a prefix
    let mut issues: Vec<IssueFull> = Vec::new();
    if !args.ids.is_empty() {
        let store = Store::open(&gage_store::store_path()).context("gage resolve")?;
        let issue_store = IssueStore::from(&store);
        let mut errors = 0;
        for prefix in &args.ids {
            match issue_store.get(prefix) {
                Ok(issue) => issues.push(issue),
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
    let ids: Vec<String> = issues.iter().map(|i| i.id.clone()).collect();

    cli::log::info(dialog::wrap_text(
        &Term::stdout(),
        "You are about to start an interactive session to resolve issues. \
         Standard token usage applies for the selected model.",
    ))?;
    if issues.is_empty() {
        dialog::step_value("Issues", "all pending and open")?;
    } else {
        dialog::step_list(
            "Issues",
            issues
                .iter()
                .map(|i| format!("{} {}", short_uuid(&i.id), i.title)),
        )?;
    }

    let model = match &args.model {
        Some(m) => {
            cli::log::step(format!("Model\n{}", style(m).dim()))?;
            Some(m.clone())
        }
        None if args.yes => {
            cli::log::step(format!("Model\n{}", style("Default").dim()))?;
            None
        }
        None => model_prompt::prompt_model_ranked()?,
    };

    if !args.yes {
        let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    *setup = Some(ResolveSetup { ids, model });
    Ok(DialogResult::from("Starting resolve session"))
}
