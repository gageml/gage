//! `gage agent` — UI surface over the `gage-agent` crate.

use std::collections::HashSet;
use std::time::Duration;

use clap::Args;
use gage_agent::{AgentBuilder, ToolPolicy};
use gage_claude::session;
use indicatif::{ProgressBar, ProgressStyle};

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
    let mut builder = AgentBuilder::new();
    if !args.tools.is_empty() {
        let mut allow = ToolPolicy::default_tools();
        allow.extend(args.tools);
        match ToolPolicy::tools(allow, vec![]) {
            Ok(resolved) => builder = builder.tools(resolved),
            Err(e) => {
                eprintln!("gage agent: --tools: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Some(name) = args.project {
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
    match agent.run(args.prompt) {
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
