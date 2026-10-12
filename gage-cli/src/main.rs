use std::sync::OnceLock;
use std::time::Duration;

use clap::{Parser, Subcommand};
use humantime::DurationError;
use tokio_util::sync::CancellationToken;

use crate::source::SOURCE_HELP;

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
mod cmd_attachment;
mod cmd_backup;
mod cmd_config;
mod cmd_dataset;
mod cmd_fetch;
mod cmd_index;
mod cmd_init;
mod cmd_issue;
mod cmd_note;
mod cmd_push;
mod cmd_query;
mod cmd_remote;
mod cmd_resolve;
mod cmd_scan;
mod cmd_session;
mod cmd_status;
mod cmd_store;
mod cmd_tag;
mod dialog;
#[cfg(test)]
mod help_lint;
mod human;
mod json;
mod limit;
mod markdown;
mod model_prompt;
mod scanner_attach;
mod scanner_spec;
mod session_filter;
mod size_limit;
mod source;
mod style;
mod target;
mod text_arg;

/// Version string baked at build time: a real semver for official release
/// builds, otherwise `git-<hash>` for source builds. See build.rs.
const VERSION: &str = include_str!(concat!(env!("OUT_DIR"), "/version.txt"));

#[derive(Parser)]
#[command(name = "gage", version = VERSION, about = "Gage CLI")]
struct Cli {
    /// Log level
    ///
    /// One of trace, debug, info, warn, or error
    #[arg(long, value_name = "LEVEL")]
    log: Option<String>,

    #[command(subcommand)]
    command: Command,
}

/// Log targets for `--log`: the crate names in underscored form
/// (clap target = crate name with `-` → `_`), plus the runtime's
/// `LOG_TARGET` for a scanner's own `log` records. Keep in sync with
/// the workspace members that actually emit tracing events.
const GAGE_LOG_TARGETS: &[&str] = &[
    gage_runtime::LOG_TARGET,
    "gage_claude",
    "gage_cli",
    "gage_core",
    "gage_index",
    "gage_log",
    "gage_lsp",
    "gage_mcp",
    "gage_query",
    "gage_registry",
    "gage_runtime",
    "gage_scan",
    "gage_scan_ui",
    "gage_tui",
];

#[derive(Subcommand)]
enum Command {
    /// Register Gage with Claude Code
    Init(cmd_init::InitArgs),

    /// Run scanners on sessions
    Scan(cmd_scan::ScanArgs),

    /// Resolve issues in a Claude Code session
    ///
    /// Starts an interactive session that resolves pending issues,
    /// then walks through open issues as directed.
    Resolve(cmd_resolve::ResolveArgs),

    /// Manage sessions
    Session {
        /// Session source
        #[arg(
            short = 's',
            long,
            value_name = "SOURCE",
            long_help = format!("Session source\n\n{SOURCE_HELP}"),
            global = true,
            display_order = 1
        )]
        source: Option<String>,

        /// Operate on stored sessions
        #[arg(
            short = 'S',
            long,
            global = true,
            conflicts_with = "source",
            display_order = 2
        )]
        stored: bool,

        #[command(subcommand)]
        command: cmd_session::SessionCommand,
    },

    /// Manage datasets
    Dataset {
        #[command(subcommand)]
        command: cmd_dataset::DatasetCommand,
    },

    /// Manage attachments
    ///
    /// An attachment is a set of files added from a directory for
    /// scanners to read, such as a Claude Code config directory. It
    /// may be named, and it may target a session. Datasets hold
    /// attachments the way they hold sessions.
    Attachment {
        #[command(subcommand)]
        command: cmd_attachment::AttachmentCommand,
    },

    /// Manage issues
    Issue {
        #[command(subcommand)]
        command: cmd_issue::IssueCommand,
    },

    /// Manage notes
    Note {
        #[command(subcommand)]
        command: cmd_note::NoteCommand,
    },

    /// Manage tags
    ///
    /// A tag is a name for an object of any type. Any argument that
    /// takes an object ID accepts a tag name in its place.
    Tag {
        #[command(subcommand)]
        command: cmd_tag::TagCommand,
    },

    /// Manage remotes
    ///
    /// A remote is a Git repository the store pushes to, fetches
    /// from, or backs up to.
    Remote {
        #[command(subcommand)]
        command: cmd_remote::RemoteCommand,
    },

    /// Show Gage login, plugin, corpus, and storage status
    Status(cmd_status::StatusArgs),

    /// Manage Gage configuration
    Config {
        #[command(subcommand)]
        command: cmd_config::ConfigCommand,
    },

    /// Manage Gage store
    Store {
        #[command(subcommand)]
        command: cmd_store::StoreCommand,
    },

    /// Push objects to a remote
    ///
    /// Sends the named objects and everything they link. The whole
    /// store is sent only by `gage backup`.
    Push(cmd_push::PushArgs),

    /// Fetch objects from a remote or URL
    ///
    /// Adopts what the remote has: everything, or the named objects
    /// and what they link. Reports what could not be merged.
    Fetch(cmd_fetch::FetchArgs),

    /// Back up store to a remote
    ///
    /// Sends every object, tag, and key to a remote added with
    /// `--backup`. Restoring is `gage fetch`.
    Backup(cmd_backup::BackupArgs),

    /// Query sessions with SQL
    Query(cmd_query::QueryArgs),

    /// Update Gage index
    Index(cmd_index::IndexArgs),

    /// Start MCP server
    Mcp,
}

fn parse_duration(s: &str) -> Result<Duration, DurationError> {
    humantime::parse_duration(s)
}

// TODO: re-enable MultiProgress tracing writer with AtomicBool flag
// for commands that use progress bars
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
        Command::Mcp => Some(gage_log::init("mcp").expect("init log dir")),
        Command::Scan(_) => {
            cmd_scan::init_logging();
            None
        }
        _ => {
            init_logging();
            None
        }
    };
    let cmd = async {
        match cli.command {
            Command::Config { command } => cmd_config::run(command),
            Command::Store { command } => cmd_store::run(command),
            Command::Init(args) => cmd_init::run(args),
            Command::Status(args) => cmd_status::run(args).await,
            Command::Note { command } => match command {
                cmd_note::NoteCommand::List(args) => cmd_note::list(args).await,
                cmd_note::NoteCommand::Add(args) => cmd_note::add(args),
                cmd_note::NoteCommand::Show(args) => cmd_note::show(args).await,
                cmd_note::NoteCommand::Edit(args) => cmd_note::edit(args),
                cmd_note::NoteCommand::Delete(args) => cmd_note::delete(args),
            },
            Command::Tag { command } => match command {
                cmd_tag::TagCommand::Add(args) => cmd_tag::add(args),
                cmd_tag::TagCommand::List(args) => cmd_tag::list(args),
                cmd_tag::TagCommand::Delete(args) => cmd_tag::delete(args),
            },
            Command::Remote { command } => match command {
                cmd_remote::RemoteCommand::Add(args) => cmd_remote::add(args),
                cmd_remote::RemoteCommand::List => cmd_remote::list(),
                cmd_remote::RemoteCommand::Delete(args) => cmd_remote::delete(args),
            },
            Command::Session {
                source,
                stored,
                command,
            } => match command {
                cmd_session::SessionCommand::List(args) => {
                    cmd_session::list(source, stored, args).await
                }
                cmd_session::SessionCommand::Add(args) => {
                    cmd_session::add(source, stored, args).await
                }
                cmd_session::SessionCommand::Remove(args) => {
                    cmd_session::remove(source, stored, args)
                }
                cmd_session::SessionCommand::Delete(args) => {
                    cmd_session::delete(source, stored, args).await
                }
                cmd_session::SessionCommand::View(args) => {
                    cmd_session::view(source, stored, args).await
                }
                cmd_session::SessionCommand::Move(args) => cmd_session::move_(args),
            },
            Command::Dataset { command } => match command {
                cmd_dataset::DatasetCommand::Add(args) => cmd_dataset::add(args).await,
                cmd_dataset::DatasetCommand::Refresh(args) => cmd_dataset::refresh(args).await,
                cmd_dataset::DatasetCommand::List(args) => cmd_dataset::list(args).await,
                cmd_dataset::DatasetCommand::Show(args) => cmd_dataset::show(args),
                cmd_dataset::DatasetCommand::Delete(args) => cmd_dataset::delete(args),
            },
            Command::Attachment { command } => match command {
                cmd_attachment::AttachmentCommand::Add(args) => cmd_attachment::add(args).await,
                cmd_attachment::AttachmentCommand::List(args) => cmd_attachment::list(args).await,
                cmd_attachment::AttachmentCommand::Show(args) => cmd_attachment::show(args),
                cmd_attachment::AttachmentCommand::Remove(args) => cmd_attachment::remove(args),
            },
            Command::Issue { command } => match command {
                cmd_issue::IssueCommand::List(args) => cmd_issue::list(args).await,
                cmd_issue::IssueCommand::Show(args) => cmd_issue::show(args).await,
                cmd_issue::IssueCommand::Add(args) => cmd_issue::add(args),
                cmd_issue::IssueCommand::Delete(args) => cmd_issue::delete(args),
                cmd_issue::IssueCommand::Close(args) => cmd_issue::close(args),
                cmd_issue::IssueCommand::Open(args) => cmd_issue::open(args),
                cmd_issue::IssueCommand::Comment(args) => cmd_issue::comment(args),
            },
            Command::Scan(args) => cmd_scan::main(args).await,
            Command::Resolve(args) => cmd_resolve::run(args),
            Command::Mcp => {
                if let Err(e) = run_mcp().await {
                    eprintln!("gage mcp: {e}");
                    std::process::exit(1);
                }
            }
            Command::Query(args) => cmd_query::main(args).await,
            Command::Index(args) => cmd_index::run(args).await,
            Command::Push(args) => cmd_push::main(args).await,
            Command::Fetch(args) => cmd_fetch::main(args),
            Command::Backup(args) => cmd_backup::main(args),
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

async fn run_mcp() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::{Arc, Mutex};

    let path = gage_store::store_path();
    let store = gage_store::Store::open(&path)
        .map_err(|e| format!("open store at {}: {e}", path.display()))?;
    let store = Arc::new(Mutex::new(store));
    let context = gage_query::ContextBuilder::new(Some(Arc::clone(&store)))
        .build()
        .await;
    let writer = gage_mcp::StoreWriterConfig {
        store,
        author: author::resolve_author(None),
    };
    // The names an interactive session may write notes under; scanner
    // output names stay with the scanners that declare them
    let note_names = std::collections::BTreeMap::from([(
        "comment".to_string(),
        "A remark about a session or the work at hand".to_string(),
    )]);
    let spec = gage_mcp::ToolSpec {
        gage: vec![
            gage_mcp::GageTool::Query(gage_mcp::QueryConfig {
                context: Arc::new(context),
            }),
            gage_mcp::GageTool::IssueWrite(gage_mcp::IssueWriteConfig::with_store(
                writer.clone(),
                "user-issue".to_string(),
                gage_store::IssueStatus::Open,
            )),
            gage_mcp::GageTool::NoteWrite(gage_mcp::NoteWriteConfig::with_store(
                writer.clone(),
                note_names,
            )),
            gage_mcp::GageTool::IssueUpdate(writer.clone()),
            gage_mcp::GageTool::IssueComment(writer.clone()),
            gage_mcp::GageTool::IssuePendingResolve(writer),
        ],
        custom: Vec::new(),
    };
    gage_mcp::serve_stdio(spec).await
}
