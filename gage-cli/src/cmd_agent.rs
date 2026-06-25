//! `gage agent` — UI surface over the `gage-agent` crate.

use std::collections::HashSet;
use std::time::Duration;

use clap::Args;
use gage_agent::{AgentBuilder, ToolPolicy};
use gage_claude::session;
use indicatif::{ProgressBar, ProgressStyle};

#[derive(Args)]
pub struct AgentArgs {
    /// Sessions visible to the agent (ID or prefix). Omit for full
    /// corpus.
    #[arg(value_name = "SESSION")]
    pub sessions: Vec<String>,

    /// Project slug used to archive the session under `~/.gage/claude/<project>/`
    #[arg(short = 'p', long = "project")]
    pub project: Option<String>,

    /// Initial prompt passed to `claude` as its positional argument
    #[arg(short = 'q', long = "prompt")]
    pub prompt: Option<String>,

    /// Additional MCP tools to expose, comma-separated short names
    /// (e.g. `IssueOpen,IssueGet`). Use `*` to allow every tool the
    /// gage MCP server provides. Added to the default `Query` allow
    /// list; no deny support
    #[arg(long = "tools", value_name = "LIST", value_delimiter = ',')]
    pub tools: Vec<String>,
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
