//! `gage agent` — UI surface over the `gage-agent` crate.

use std::collections::HashSet;
use std::time::Duration;

use clap::Args;
use cliclack as cli;
use gage_agent::{AgentBuilder, TOOL_NAMES, ToolPolicy};
use gage_claude::session;
use indicatif::{ProgressBar, ProgressStyle};

use crate::dialog::{self, DialogError, DialogResult};

#[derive(Args)]
pub struct AgentArgs {
    /// Run agent for specified session IDs only (or prefix)
    ///
    /// If omitted, all available sessions are available to the agent.
    #[arg(value_name = "SESSION")]
    pub sessions: Vec<String>,

    /// Agent name
    ///
    /// This value is used as the session project name in listings to
    /// differentiate agent session types. Defaults to "default".
    #[arg(short, long)]
    pub name: Option<String>,

    /// Initial agent prompt
    #[arg(short, long)]
    pub prompt: Option<String>,

    /// Tools available to the agent (comma-separated list)
    ///
    /// Use '*' to enable all tools.
    #[arg(short, long, value_name = "LIST", value_delimiter = ',')]
    pub tools: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

pub fn run(args: AgentArgs) {
    let sessions = match resolve_sessions(&args.sessions) {
        Ok(s) => s,
        Err(()) => std::process::exit(1),
    };

    let mut tools = args.tools.clone();
    let mut prompt = args.prompt.clone();
    if !args.yes && args.tools.is_empty() && args.prompt.is_none() {
        let mut tools_out: Vec<String> = Vec::new();
        let mut prompt_out: Option<String> = None;
        let mut completed = false;
        dialog::run("Run agent", || {
            let r = collect_dialog(&mut tools_out, &mut prompt_out);
            if r.is_ok() {
                completed = true;
            }
            r
        });
        if !completed {
            std::process::exit(1);
        }
        tools = tools_out;
        prompt = prompt_out;
    }

    let mut builder = AgentBuilder::new();
    if !tools.is_empty() {
        let mut allow = ToolPolicy::default_tools();
        allow.extend(tools);
        match ToolPolicy::tools(allow, vec![]) {
            Ok(resolved) => builder = builder.tools(resolved),
            Err(e) => {
                eprintln!("gage agent: --tools: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Some(name) = args.name {
        builder = builder.name(name);
    }
    if let Some(ids) = sessions {
        builder = builder.sessions(ids);
    }
    let mut agent = builder.build();
    let spinner = start_spinner("Starting agent");
    if let Err(e) = agent.init() {
        spinner.finish_and_clear();
        eprintln!("gage agent: {e}");
        std::process::exit(1);
    }
    spinner.finish_and_clear();
    match agent.run(prompt) {
        Ok(status) => {
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        Err(e) => {
            eprintln!("gage agent: {e}");
            std::process::exit(1);
        }
    }
}

fn collect_dialog(
    tools: &mut Vec<String>,
    prompt: &mut Option<String>,
) -> Result<DialogResult, DialogError> {
    let names: Vec<&'static str> = TOOL_NAMES
        .iter()
        .copied()
        .filter(|n| *n != "Query")
        .collect();
    let mut ms = cli::multiselect("Tools").required(false);
    for (i, name) in names.iter().enumerate() {
        ms = ms.item(i, *name, "");
    }
    let picks: Vec<usize> = ms.interact()?;
    *tools = picks
        .iter()
        .map(|&i| {
            names
                .get(i)
                .expect("selected holds positions in names")
                .to_string()
        })
        .collect();

    let entered: String = cli::input("Initial prompt")
        .placeholder("Optional")
        .required(false)
        .interact()?;
    *prompt = if entered.is_empty() {
        None
    } else {
        Some(entered)
    };
    Ok("Starting agent".into())
}

fn start_spinner(message: &str) -> ProgressBar {
    let bar = ProgressBar::new_spinner();
    bar.set_style(ProgressStyle::with_template("{spinner:.magenta} {msg}").unwrap());
    bar.enable_steady_tick(Duration::from_millis(120));
    bar.set_message(message.to_string());
    bar
}

/// Resolve user-supplied session prefixes to full ids. `Ok(None)`
/// means no selectors were given (full-corpus sandbox); `Ok(Some(set))`
/// is the explicit allowlist. Diagnostics for each unresolved prefix
/// are written to stderr; any failure yields `Err(())`.
fn resolve_sessions(prefixes: &[String]) -> Result<Option<HashSet<String>>, ()> {
    if prefixes.is_empty() {
        return Ok(None);
    }
    let mut ids = HashSet::new();
    let mut failed = false;
    for prefix in prefixes {
        match session::one_session(prefix) {
            Ok(s) => {
                ids.insert(s.id);
            }
            Err(e) => {
                eprintln!("{e}");
                failed = true;
            }
        }
    }
    if failed {
        return Err(());
    }
    Ok(Some(ids))
}
