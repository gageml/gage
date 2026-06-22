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
mod scan_progress;
mod style;
mod target_content;

/// Version string baked at build time: a real semver for official release
/// builds, otherwise `git-<hash>` for source builds. See build.rs.
const VERSION: &str = include_str!(concat!(env!("OUT_DIR"), "/version.txt"));

#[derive(Parser)]
#[command(name = "gage", version = VERSION, about = "Gage CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Setup Gage (register with Claude Code)
    Init(cmd_init::InitArgs),

    /// Run scanners on sessions
    Scan(cmd_scan::ScanArgs),

    /// Run Claude to perform Gage tasks
    Agent(cmd_agent::AgentArgs),

    /// Manage sessions
    Session {
        /// Operate on agent sessions under `<gage_home>/claude` instead
        /// of the user's Claude Code sessions
        #[arg(short = 'A', long, global = true)]
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

    /// Back up local state to every configured remote
    Push(cmd_sync::PushArgs),

    /// Copy a remote's tree into a local inspection directory
    Pull(cmd_sync::PullArgs),

    /// Manage Gage configuration
    Config {
        #[command(subcommand)]
        command: cmd_config::ConfigCommand,
    },

    /// Reconcile the derived session store and text index
    Index(cmd_index::IndexArgs),

    /// Start the MCP server (stdio transport)
    Mcp,

    /// Run tests in scanner modules
    Test(cmd_test::TestArgs),
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
    init_logging();
    let cli = Cli::parse();
    let cmd = async {
        match cli.command {
            Command::Agent(args) => cmd_agent::run(args),
            Command::Config { command } => cmd_config::run(command),
            Command::Init(args) => cmd_init::run(args),
            Command::Note { command } => match command {
                cmd_note::NoteCommand::List(args) => cmd_note::list(args),
                cmd_note::NoteCommand::Add(args) => cmd_note::add(args),
                cmd_note::NoteCommand::Comment(args) => cmd_note::comment(args),
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
                    cmd_session::SessionCommand::List(args) => cmd_session::list(args).await,
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
                cmd_issue::IssueCommand::Reopen(args) => cmd_issue::reopen(args),
                cmd_issue::IssueCommand::Comment(args) => cmd_issue::comment(args),
            },
            Command::Test(args) => cmd_test::run(args).await,
            Command::Scan(args) => cmd_scan::run(args).await,
            Command::Mcp => {
                if let Err(e) = gage_mcp::serve_stdio().await {
                    eprintln!("gage mcp: {e}");
                    std::process::exit(1);
                }
            }
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
