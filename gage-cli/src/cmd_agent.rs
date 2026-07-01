//! `gage agent` — UI surface over the `gage-agent` crate.

use std::time::Duration;

use std::sync::Arc;

use clap::Args;
use cliclack as cli;
use gage_agent::{AgentBuilder, TOOL_NAMES, ToolPolicy};
use gage_claude::session::{self, SessionListBuilder};
use gage_db::scan::{Scan, insert_scan, insert_scan_session};
use gage_mcp::{McpHost, ToolSpec, build_mcp_service};
use indicatif::{ProgressBar, ProgressStyle};

use crate::dialog::{self, DialogError, DialogResult};

#[derive(Args)]
pub struct AgentArgs {
    /// Scope agent to specified session IDs only (or prefix)
    ///
    /// If omitted, all available sessions are available to the agent.
    #[arg(value_name = "SESSION", conflicts_with_all = ["limit", "days", "all"])]
    pub sessions: Vec<String>,

    /// Scope to most recent N sessions
    #[arg(short = 'n', long, value_name = "N", conflicts_with_all = ["days", "all"])]
    pub limit: Option<usize>,

    /// Scope to sessions modified in past N days (default 30)
    #[arg(short, long, value_name = "N", conflicts_with = "all")]
    pub days: Option<u32>,

    /// Scope to all sessions
    #[arg(short, long)]
    pub all: bool,

    /// Agent name
    ///
    /// This value is used as the session project name in listings to
    /// differentiate agent session types. Defaults to "default".
    #[arg(long)]
    pub name: Option<String>,

    /// Initial agent prompt
    #[arg(short, long)]
    pub prompt: Option<String>,

    /// Agent model
    #[arg(short, long)]
    pub model: Option<String>,

    /// Gage tools to expose to the agent (comma-separated list)
    ///
    /// Tools: Query, NoteWrite, IssueClose, IssueComment, IssueOpen
    ///
    /// Use '*' to enable all tools.
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
    let sessions = match resolve_sessions(&args) {
        Ok(s) => s,
        Err(()) => std::process::exit(1),
    };

    let mut tools = args.gage_tools.clone();
    let prompt = args.prompt.clone();
    if !args.yes && args.gage_tools.is_empty() && args.prompt.is_none() {
        let mut tools_out: Vec<String> = Vec::new();
        let mut completed = false;
        dialog::run("Run agent", || {
            let r = collect_dialog(&mut tools_out);
            if r.is_ok() {
                completed = true;
            }
            r
        });
        if !completed {
            std::process::exit(1);
        }
        tools = tools_out;
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

    // Record a scan row keyed to this invocation and populate
    // `scan_session` with the ids the agent can read. The gage MCP
    // server reads `GAGE_SCAN_ID` from the process env to scope the
    // agent's `Query` context and to auto-link any notes / issues
    // the agent writes. No `scan_scanner` row is inserted, so `gage
    // scan list` filters this scan out of the scanner-scan listing.
    let scan_id = match register_scan(sessions) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage agent: {e}");
            std::process::exit(1);
        }
    };
    // SAFETY: fresh scan id for this process; nothing else reads it
    // yet, and the process ends when the agent exits.
    unsafe {
        std::env::set_var("GAGE_SCAN_ID", &scan_id);
    }

    let mut builder = AgentBuilder::new()
        .tools(resolved_tools)
        .mcp_url(mcp_url)
        .scan_id(&scan_id);
    if let Some(name) = args.name {
        builder = builder.name(name);
    }
    if let Some(model) = args.model {
        builder = builder.model(model);
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

fn collect_dialog(tools: &mut Vec<String>) -> Result<DialogResult, DialogError> {
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

    Ok("Starting agent".into())
}

fn start_spinner(message: &str) -> ProgressBar {
    let bar = ProgressBar::new_spinner();
    bar.set_style(ProgressStyle::with_template("{spinner:.magenta} {msg}").unwrap());
    bar.enable_steady_tick(Duration::from_millis(120));
    bar.set_message(message.to_string());
    bar
}

/// Insert a fresh `scan` row and populate `scan_session` with the ids
/// the agent has access to.
fn register_scan(ids: Vec<String>) -> Result<String, String> {
    let conn = gage_db::db::open_db().map_err(|e| format!("open db: {e}"))?;
    let scan_id = gage_core::uuid::new_uuid();
    insert_scan(
        &conn,
        &Scan {
            id: scan_id.clone(),
            created: gage_core::datetime::now_ms(),
            metadata: None,
        },
    )
    .map_err(|e| format!("insert scan: {e}"))?;
    for sid in &ids {
        insert_scan_session(&conn, &scan_id, sid)
            .map_err(|e| format!("insert scan_session: {e}"))?;
    }
    Ok(scan_id)
}

/// Resolve session scope to a concrete list of ids. Explicit prefixes
/// take precedence; otherwise falls back to `--limit` / `--days` /
/// `--all`, defaulting to sessions modified in the past 30 days.
/// Diagnostics for each unresolved prefix are written to stderr; any
/// failure yields `Err(())`.
fn resolve_sessions(args: &AgentArgs) -> Result<Vec<String>, ()> {
    if !args.sessions.is_empty() {
        let mut ids = Vec::new();
        let mut failed = false;
        for prefix in &args.sessions {
            match session::one_session(prefix) {
                Ok(s) => ids.push(s.id),
                Err(e) => {
                    eprintln!("{e}");
                    failed = true;
                }
            }
        }
        if failed {
            return Err(());
        }
        return Ok(ids);
    }

    let days = if args.all || args.limit.is_some() {
        None
    } else {
        Some(args.days.unwrap_or(30))
    };

    let mut builder = SessionListBuilder::new();
    if let Some(d) = days {
        builder = builder.since(Duration::from_secs(u64::from(d) * 86_400));
    }
    if let Some(n) = args.limit {
        builder = builder.limit(n);
    }
    Ok(builder.build().into_iter().map(|s| s.id).collect())
}
