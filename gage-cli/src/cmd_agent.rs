//! `gage agent` — run an agent def declared in a scanner manifest.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Args;
use cliclack as cli;
use console::style;
use gage_agent::AgentBuilder;
use gage_claude::session::{self, SessionInfo, SessionListBuilder};
use gage_registry::scanner::{Scanner, ScannerDef, ScannerRegistry};
use gage_scan::agent_def::{AgentDefOutcome, run_agent_def};
use indicatif::{ProgressBar, ProgressStyle};
use tabled::{
    Table,
    settings::{Color, Style, Width, object::Rows, peaker::Priority},
};

use crate::dialog::{self, DialogError, DialogResult};

#[derive(Args)]
pub struct AgentArgs {
    /// Agent to run, as <scanner>::<fn> or bare <fn>
    ///
    /// A bare fn name must match exactly one declared agent.
    #[arg(value_name = "AGENT", required_unless_present = "list")]
    pub agent: Option<String>,

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

    /// Run interactively in a Claude Code session
    #[arg(short, long)]
    pub interactive: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,

    /// List available agents
    #[arg(short, long)]
    pub list: bool,
}

pub async fn run(args: AgentArgs) {
    let registry = ScannerRegistry::load();

    if args.list {
        list_agents(&registry);
        return;
    }

    let agent_ref = args.agent.as_deref().expect("clap requires AGENT");
    let (def, fn_name) = match resolve_agent(&registry, agent_ref) {
        Ok(resolved) => resolved,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    };

    let selected: Arc<[SessionInfo]> = match resolve_sessions(&args) {
        Ok(s) => Arc::from(s.into_boxed_slice()),
        Err(()) => std::process::exit(1),
    };

    let mut output = None;
    dialog::run_async("Run agent", || {
        run_dialog(&args, def, fn_name, selected, &mut output)
    })
    .await;
    if let Some(text) = output {
        println!("{text}");
    }
}

async fn run_dialog(
    args: &AgentArgs,
    def: &ScannerDef,
    fn_name: &str,
    selected: Arc<[SessionInfo]>,
    output: &mut Option<String>,
) -> Result<DialogResult, DialogError> {
    let agent_name = format!("{}::{fn_name}", def.name);
    cli::log::step(format!("Agent\n{}", style(&agent_name).dim()))?;
    cli::log::step(format!(
        "Sessions\n{}",
        style(sessions_label(args, &selected)).dim()
    ))?;

    if !args.yes {
        let confirmed = cli::confirm("Run this agent?")
            .initial_value(true)
            .interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    let db = Arc::new(Mutex::new(gage_db::db::open_db().unwrap()));
    let scanner = Scanner { def, params: None };

    let spinner = if args.interactive {
        None
    } else {
        Some(start_run_spinner(&agent_name))
    };
    let result = run_agent_def(db, scanner, fn_name, selected, args.interactive).await;
    let elapsed = spinner.as_ref().map(|s| s.elapsed());
    if let Some(spinner) = spinner {
        spinner.finish_and_clear();
    }
    let outcome = result.map_err(|e| DialogError::Other(anyhow::anyhow!("{e}")))?;

    match outcome {
        AgentDefOutcome::Headless(result) => {
            if result.is_error || result.exit_code != 0 {
                // A failed dialog exits the process, so the failure
                // output prints here, before the outro.
                println!("{}", result.text);
                if !result.stderr.is_empty() {
                    eprintln!("{}", result.stderr);
                }
                return Err(DialogError::Failed(format!("{agent_name} failed")));
            }
            *output = Some(result.text);
            let elapsed = elapsed.map(fmt_elapsed_secs).unwrap_or_default();
            Ok(DialogResult::from(format!("Agent completed in {elapsed}")))
        }
        AgentDefOutcome::Interactive(run) => {
            let spec = &run.spec;
            let mut builder = AgentBuilder::new()
                .tools(spec.tools.clone())
                .prompt(spec.prompt.clone());
            if let Some(name) = &spec.name {
                builder = builder.name(name.clone());
            }
            if let Some(model) = &spec.model {
                builder = builder.model(model.clone());
            }
            if let Some(url) = &spec.mcp_url {
                builder = builder.mcp_url(url.clone());
            }
            let mut agent = builder.build();
            let spinner = start_spinner("Starting agent");
            let init_result = agent.init();
            spinner.finish_and_clear();
            init_result.map_err(|e| DialogError::Other(anyhow::anyhow!("{e}")))?;
            let status = agent
                .run()
                .map_err(|e| DialogError::Other(anyhow::anyhow!("{e}")))?;
            // `run` (MCP service, dispatcher, run context) must
            // outlive the interactive session; `finish` consumes it
            // and records the run summary on the proxy scan.
            run.finish(!status.success())
                .map_err(|e| DialogError::Other(anyhow::anyhow!("{e}")))?;
            if !status.success() {
                return Err(DialogError::Failed(format!(
                    "{agent_name} exited with status {}",
                    status.code().unwrap_or(1)
                )));
            }
            Ok(DialogResult::from(format!("{agent_name} session ended")))
        }
    }
}

/// Session-scope description mirroring the scan dialog's label
/// conventions.
fn sessions_label(args: &AgentArgs, selected: &[SessionInfo]) -> String {
    if !args.sessions.is_empty() {
        return selected
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
    if args.all {
        "all".to_string()
    } else if let Some(n) = args.limit {
        format!("{n} most recent")
    } else {
        let d = args.days.unwrap_or(30);
        format!("last {d} day{}", if d == 1 { "" } else { "s" })
    }
}

fn list_agents(registry: &ScannerRegistry) {
    let rows: Vec<Vec<String>> = registry
        .list()
        .iter()
        .flat_map(|def| {
            def.agents.iter().map(|(fn_name, description)| {
                vec![
                    console::style(format!("{}::{fn_name}", def.name))
                        .yellow()
                        .to_string(),
                    console::style(description).dim().to_string(),
                ]
            })
        })
        .collect();
    if rows.is_empty() {
        println!("No agents declared");
        return;
    }

    let header: Vec<String> = ["Agent", "Description"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let term_width = console::Term::stdout().size().1 as usize;
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .with(
            Width::wrap(term_width)
                .keep_words(true)
                .priority(Priority::max(true)),
        )
        .modify(Rows::first(), Color::FG_BRIGHT_YELLOW)
        .to_string();
    println!("{table}");
}

/// Resolve an agent reference to its scanner def and fn name. Accepts
/// the full `<scanner>::<fn>` form or a bare fn name; a bare name must
/// match exactly one declared agent across all scanners.
fn resolve_agent<'a>(
    registry: &'a ScannerRegistry,
    agent_ref: &'a str,
) -> Result<(&'a ScannerDef, &'a str), String> {
    if let Some((scanner_name, fn_name)) = agent_ref.split_once("::") {
        let Some(def) = registry.get_def(scanner_name) else {
            return Err(format!("gage agent: no scanner named '{scanner_name}'"));
        };
        if !def.agents.contains_key(fn_name) {
            let mut msg =
                format!("gage agent: scanner '{scanner_name}' declares no agent '{fn_name}'");
            match def.agents.keys().collect::<Vec<_>>() {
                keys if keys.is_empty() => msg.push_str("\n(no agents declared)"),
                keys => {
                    msg.push_str("\ndeclared agents:");
                    for k in keys {
                        msg.push_str(&format!("\n  {scanner_name}::{k}"));
                    }
                }
            }
            return Err(msg);
        }
        return Ok((def, fn_name));
    }

    let matches: Vec<&ScannerDef> = registry
        .list()
        .into_iter()
        .filter(|def| def.agents.contains_key(agent_ref))
        .collect();
    match matches.as_slice() {
        [] => Err(format!("gage agent: no agent named '{agent_ref}'")),
        [def] => Ok((def, agent_ref)),
        defs => {
            let mut msg = format!("gage agent: '{agent_ref}' matches multiple agents:");
            for def in defs {
                msg.push_str(&format!("\n  {}::{agent_ref}", def.name));
            }
            Err(msg)
        }
    }
}

fn start_run_spinner(agent_name: &str) -> ProgressBar {
    let bar = ProgressBar::new_spinner();
    let style = ProgressStyle::with_template("{spinner:.magenta} Running {msg} {gage_elapsed}")
        .unwrap()
        .with_key(
            "gage_elapsed",
            |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                w.write_str(&fmt_elapsed_secs(state.elapsed())).unwrap();
            },
        );
    bar.set_style(style);
    bar.enable_steady_tick(Duration::from_millis(120));
    bar.set_message(agent_name.to_string());
    bar
}

/// Whole-second resolution; agent runs take seconds, so millisecond
/// values are noise.
fn fmt_elapsed_secs(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

fn start_spinner(message: &str) -> ProgressBar {
    let bar = ProgressBar::new_spinner();
    bar.set_style(ProgressStyle::with_template("{spinner:.magenta} {msg}").unwrap());
    bar.enable_steady_tick(Duration::from_millis(120));
    bar.set_message(message.to_string());
    bar
}

/// Resolve session scope to concrete sessions. Explicit prefixes take
/// precedence; otherwise falls back to `--limit` / `--days` / `--all`,
/// defaulting to sessions modified in the past 30 days. Diagnostics
/// for each unresolved prefix are written to stderr; any failure
/// yields `Err(())`.
fn resolve_sessions(args: &AgentArgs) -> Result<Vec<SessionInfo>, ()> {
    if !args.sessions.is_empty() {
        let mut out = Vec::new();
        let mut failed = false;
        for prefix in &args.sessions {
            match session::one_session(prefix) {
                Ok(s) => out.push(s),
                Err(e) => {
                    eprintln!("{e}");
                    failed = true;
                }
            }
        }
        if failed {
            return Err(());
        }
        return Ok(out);
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
    Ok(builder.build().into_iter().collect())
}
