//! `gage agent` — UI surface over the `gage-agent` crate.

use std::collections::HashSet;
use std::time::Duration;

use std::sync::Arc;

use clap::Args;
use cliclack as cli;
use gage_agent::{AgentBuilder, TOOL_NAMES, ToolPolicy};
use gage_claude::session;
use gage_mcp::{McpHost, ToolSpec, build_mcp_service};
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

    /// Model passed to the child `claude` as `--model`
    #[arg(short, long)]
    pub model: Option<String>,

    /// Gage tools to expose to the agent (comma-separated list)
    ///
    /// Mirrors `call_agent.gage_tools(...)`. Use '*' to enable all
    /// built-in Gage tools.
    #[arg(
        short = 't',
        long = "tools",
        value_name = "LIST",
        value_delimiter = ','
    )]
    pub gage_tools: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

pub async fn run(args: AgentArgs) {
    let sessions = match resolve_sessions(&args.sessions) {
        Ok(s) => s,
        Err(()) => std::process::exit(1),
    };

    let mut tools = args.gage_tools.clone();
    let mut prompt = args.prompt.clone();
    if !args.yes && args.gage_tools.is_empty() && args.prompt.is_none() {
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

    let resolved_tools = match ToolPolicy::tools(tools, vec![]) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("gage agent: --tools: {e}");
            std::process::exit(1);
        }
    };

    // Start the in-process MCP host and register a per-call service
    // exposing the resolved Gage tool set. The child claude connects to
    // this URL via `--mcp-config`. Holding the handle for the duration
    // of the run keeps the registration alive.
    let host = match McpHost::start().await {
        Ok(h) => Arc::new(h),
        Err(e) => {
            eprintln!("gage agent: start mcp host: {e}");
            std::process::exit(1);
        }
    };
    let spec = ToolSpec {
        gage_tools: resolved_tools.clone(),
        custom_tools: Vec::new(),
    };
    let _service_handle = host.register(build_mcp_service(spec));
    let mcp_url = _service_handle.url().to_string();

    let mut builder = AgentBuilder::new().tools(resolved_tools).mcp_url(mcp_url);
    if let Some(name) = args.name {
        builder = builder.name(name);
    }
    if let Some(model) = args.model {
        builder = builder.model(model);
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
            drop(_service_handle);
            drop(host);
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
    let names: Vec<&'static str> = TOOL_NAMES.to_vec();
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
