//! `gage resolve` — resolve issues in an interactive session the
//! default driver starts in its harness.

use anyhow::Context;
use clap::Args;
use cliclack as cli;
use gage_core::uuid::short_uuid;
use gage_session::{SkillMode, SkillSession};
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
    let session = SkillSession {
        name: "Resolve Gage issues",
        skill: "gage:resolve",
        args: &setup.ids,
        model: setup.model.as_deref(),
        extra_args: &args.extra_args,
        mode: SkillMode::Interactive,
    };
    source::run_skill_session("gage resolve", &session)
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

    let model = model_prompt::model_step(args.model.as_deref(), args.yes)?;

    if !args.yes {
        dialog::usage_notice(
            "You are about to start an interactive session to resolve issues. \
             Standard token usage applies for the selected model.",
        )?;
        dialog::confirm(false, true)?;
    }

    *setup = Some(ResolveSetup { ids, model });
    Ok(DialogResult::from("Starting resolve session"))
}
