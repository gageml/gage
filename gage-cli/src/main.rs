use std::sync::OnceLock;
use std::time::Duration;

use clap::{Parser, Subcommand};
use humantime::DurationError;
use tokio_util::sync::CancellationToken;

static PANIC_TOKEN: OnceLock<CancellationToken> = OnceLock::new();

pub fn panic_token() -> &'static CancellationToken {
    PANIC_TOKEN.get().expect("panic token installed at main")
}

fn install_panic_hook() {
    PANIC_TOKEN
        .set(CancellationToken::new())
        .expect("install_panic_hook called once");
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        if let Some(tok) = PANIC_TOKEN.get() {
            tok.cancel();
        }
    }));
}

mod author;
mod cmd_agent;
mod cmd_config;
mod cmd_index;
mod cmd_init;
mod cmd_issue;
mod cmd_note;
mod cmd_query;
mod cmd_scan;
mod cmd_session;
mod cmd_sync;
mod cmd_test;
mod dialog;
mod human;
mod limit;
mod markdown;
mod style;
mod target_content;

/// Version string baked at build time: a real semver for official release
/// builds, otherwise `git-<hash>` for source builds. See build.rs.
const VERSION: &str = include_str!(concat!(env!("OUT_DIR"), "/version.txt"));

#[derive(Parser)]
#[command(name = "gage", version = VERSION, about = "Gage CLI")]
struct Cli {
    /// Log level (trace, debug, info, warn, error)
    #[arg(long, value_name = "LEVEL")]
    log: Option<String>,

    #[command(subcommand)]
    command: Command,
}

/// Crate-name log targets for `--log`. Underscored form (clap target =
/// crate name with `-` → `_`). Keep in sync with the workspace members
/// that actually emit tracing events.
const GAGE_LOG_TARGETS: &[&str] = &[
    "gage_agent",
    "gage_claude",
    "gage_cli",
    "gage_core",
    "gage_db",
    "gage_eval",
    "gage_index",
    "gage_log",
    "gage_lsp",
    "gage_mcp",
    "gage_query",
    "gage_registry",
    "gage_runtime",
    "gage_scan",
    "gage_scan_ui",
    "gage_sync",
    "gage_tui",
];

#[derive(Subcommand)]
enum Command {
    /// Setup Gage (register with Claude Code)
    Init(cmd_init::InitArgs),

    /// Run scanners on sessions
    Scan(cmd_scan::ScanArgs),

    /// Run a scanner agent
    Agent(cmd_agent::AgentArgs),

    /// Manage sessions
    Session {
        /// Operate on agent sessions instead of Claude Code sessions
        #[arg(short = 'A', long)]
        agent: bool,

        #[command(subcommand)]
        command: cmd_session::SessionCommand,
    },

    /// Manage notes
    Note {
        #[command(subcommand)]
        command: cmd_note::NoteCommand,
    },

    /// Manage issues
    Issue {
        #[command(subcommand)]
        command: cmd_issue::IssueCommand,
    },

    /// Query sessions with SQL
    Query(cmd_query::QueryArgs),

    /// Copy Gage data to remotes
    Push(cmd_sync::PushArgs),

    /// Copy Gage data from remotes
    Pull(cmd_sync::PullArgs),

    /// Manage Gage configuration
    Config {
        #[command(subcommand)]
        command: cmd_config::ConfigCommand,
    },

    /// Update the Gage index
    Index(cmd_index::IndexArgs),

    /// Start the MCP server
    Mcp {
        #[command(subcommand)]
        command: Option<McpCommand>,
    },

    /// Run scanner tests
    Test(cmd_test::TestArgs),
}

#[derive(Subcommand)]
enum McpCommand {
    /// Serve over stdio (default)
    Stdio,

    /// Serve over HTTP at the given bind address
    Http {
        /// Address to bind, e.g. `127.0.0.1:8765`
        #[arg(short, long, default_value = "127.0.0.1:0")]
        bind: std::net::SocketAddr,
    },
}

fn parse_duration(s: &str) -> Result<Duration, DurationError> {
    humantime::parse_duration(s)
}

// TODO: re-enable MultiProgress tracing writer with AtomicBool flag
// for commands that use progress bars (see PLAN.md "After implementation")
//
// static MP: OnceLock<MultiProgress> = OnceLock::new();
//
// pub fn multi_progress() -> &'static MultiProgress {
//     MP.get().expect("logging not initialized")
// }

fn init_logging() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env("GAGE_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .init();
}

#[tokio::main]
async fn main() {
    install_panic_hook();
    let cli = Cli::parse();
    if let Some(level) = &cli.log {
        let directive = GAGE_LOG_TARGETS
            .iter()
            .map(|t| format!("{t}={level}"))
            .collect::<Vec<_>>()
            .join(",");
        // SAFETY: set_var is unsafe in edition 2024; we set this once at
        // startup, before logging init reads GAGE_LOG and before any
        // threads spawn.
        unsafe { std::env::set_var("GAGE_LOG", directive) };
    }
    let _log_guard = match &cli.command {
        Command::Mcp { .. } => Some(gage_log::init("mcp").expect("init log dir")),
        // A scan run inits its own scan-id-named log in cmd_scan; the
        // scan subcommands (list/show/view/delete) log per-process.
        Command::Scan(args) if args.command.is_some() => {
            Some(gage_log::init("scan").expect("init log dir"))
        }
        Command::Scan(_) => None,
        _ => {
            init_logging();
            None
        }
    };
    let cmd = async {
        match cli.command {
            Command::Agent(args) => cmd_agent::run(args).await,
            Command::Config { command } => cmd_config::run(command),
            Command::Init(args) => cmd_init::run(args),
            Command::Note { command } => match command {
                cmd_note::NoteCommand::List(args) => cmd_note::list(args),
                cmd_note::NoteCommand::Add(args) => cmd_note::add(args),
                cmd_note::NoteCommand::Show(args) => cmd_note::show(args).await,
                cmd_note::NoteCommand::Edit(args) => cmd_note::edit(args),
                cmd_note::NoteCommand::Delete(args) => cmd_note::delete(args),
            },
            Command::Session { agent, command } => {
                if agent {
                    let dir = gage_core::config::gage_home().join("claude");
                    // SAFETY: set_var is unsafe in edition 2024; we set
                    // this once at startup, before any threads spawn.
                    unsafe { std::env::set_var("CLAUDE_PROJECTS_DIR", &dir) };
                }
                match command {
                    cmd_session::SessionCommand::List(args) => cmd_session::list(args, agent).await,
                    cmd_session::SessionCommand::Delete(args) => cmd_session::delete(args).await,
                    cmd_session::SessionCommand::View(args) => cmd_session::view(args).await,
                    cmd_session::SessionCommand::Move(args) => cmd_session::move_(args),
                }
            }
            Command::Issue { command } => match command {
                cmd_issue::IssueCommand::List(args) => cmd_issue::list(args),
                cmd_issue::IssueCommand::Show(args) => cmd_issue::show(args),
                cmd_issue::IssueCommand::Add(args) => cmd_issue::add(args),
                cmd_issue::IssueCommand::Delete(args) => cmd_issue::delete(args),
                cmd_issue::IssueCommand::Close(args) => cmd_issue::close(args),
                cmd_issue::IssueCommand::Open(args) => cmd_issue::open(args),
                cmd_issue::IssueCommand::Comment(args) => cmd_issue::comment(args),
            },
            Command::Test(args) => cmd_test::run(args).await,
            Command::Scan(args) => cmd_scan::run(args).await,
            Command::Mcp { command } => match command.unwrap_or(McpCommand::Stdio) {
                McpCommand::Stdio => {
                    if let Err(e) = gage_mcp::serve_stdio().await {
                        eprintln!("gage mcp: {e}");
                        std::process::exit(1);
                    }
                }
                McpCommand::Http { bind } => {
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                    let addr = match gage_mcp::host::serve_http(bind, shutdown_rx).await {
                        Ok(addr) => addr,
                        Err(e) => {
                            eprintln!("gage mcp http: {e}");
                            std::process::exit(1);
                        }
                    };
                    eprintln!("gage mcp listening on http://{addr}/mcp");
                    if let Err(e) = tokio::signal::ctrl_c().await {
                        eprintln!("gage mcp http: install ctrl-c handler: {e}");
                    }
                    #[allow(clippy::unused_result_ok)]
                    shutdown_tx.send(()).ok();
                }
            },
            Command::Query(args) => cmd_query::main(args).await,
            Command::Index(args) => cmd_index::run(args).await,
            Command::Push(args) => cmd_sync::push(args).await,
            Command::Pull(args) => cmd_sync::pull(args).await,
        }
    };
    tokio::pin!(cmd);
    tokio::select! {
        () = &mut cmd => {}
        _ = panic_token().cancelled() => {
            #[allow(clippy::let_underscore_must_use)]
            let _ = tokio::time::timeout(Duration::from_secs(2), cmd).await;
            std::process::exit(1);
        }
    }
}
