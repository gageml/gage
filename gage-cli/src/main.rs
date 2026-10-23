use std::collections::BTreeMap;
use std::env;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use gage_mcp::{
    GageTool, IssueAssignConfig, IssueWriteConfig, IssuesFreeConfig, NoteWriteConfig, QueryConfig,
    SessionLiveFn, StoreWriterConfig, ToolSpec, serve_stdio,
};
use gage_query::ContextBuilder;
use gage_session::SourceOptions;
use gage_store::{IssueStatus, Store, store_path};
use humantime::DurationError;
use tokio_util::sync::CancellationToken;

use crate::error::CliError;
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
mod cmd_blame;
mod cmd_config;
mod cmd_dataset;
mod cmd_fetch;
mod cmd_index;
mod cmd_init;
mod cmd_issue;
mod cmd_note;
mod cmd_publish;
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
mod error;
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
    /// Adopts the named objects and what they link, or the whole
    /// remote with --all. Reports what could not be merged.
    Fetch(cmd_fetch::FetchArgs),

    /// Back up store to a remote
    ///
    /// Adds every session from each configured source to the store,
    /// then sends every object, tag, and key to a remote added with
    /// `--backup`. Restoring is `gage fetch`.
    Backup(cmd_backup::BackupArgs),

    /// Query sessions with SQL
    Query(cmd_query::QueryArgs),

    /// Find sessions that wrote lines of a file
    ///
    /// Follows git blame: each line's commit bounds a window, and the
    /// earliest session tool call in that window whose input carries
    /// the line's text is reported with the session lines from the
    /// prompt before it to the call
    Blame(cmd_blame::BlameArgs),

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
async fn main() -> ExitCode {
    install_panic_hook();
    // Every spinner and bar, inside a dialog or not, renders with the
    // Gage theme
    dialog::install_theme();
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
                cmd_session::SessionCommand::Move(args) => cmd_session::move_(source, stored, args),
                cmd_session::SessionCommand::View(args) => {
                    cmd_session::view(source, stored, args).await
                }
                cmd_session::SessionCommand::Find(args) => cmd_session::find(args).await,
                cmd_session::SessionCommand::Publish(args) => cmd_publish::run(args).await,
            },
            Command::Dataset { command } => match command {
                cmd_dataset::DatasetCommand::Add(args) => cmd_dataset::add(args).await,
                cmd_dataset::DatasetCommand::Refresh(args) => cmd_dataset::refresh(args),
                cmd_dataset::DatasetCommand::List(args) => cmd_dataset::list(args).await,
                cmd_dataset::DatasetCommand::Show(args) => cmd_dataset::show(args).await,
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
            Command::Mcp => run_mcp().await.context("gage mcp").map_err(CliError::from),
            Command::Query(args) => cmd_query::main(args).await,
            Command::Blame(args) => cmd_blame::run(args).await,
            Command::Index(args) => cmd_index::run(args).await,
            Command::Push(args) => cmd_push::main(args).await,
            Command::Fetch(args) => cmd_fetch::main(args),
            Command::Backup(args) => cmd_backup::main(args).await,
        }
    };
    tokio::pin!(cmd);
    let result = tokio::select! {
        result = &mut cmd => result,
        _ = panic_token().cancelled() => {
            #[allow(clippy::let_underscore_must_use)]
            let _ = tokio::time::timeout(Duration::from_secs(2), cmd).await;
            return ExitCode::FAILURE;
        }
    };
    exit_code(result)
}

/// The one place a command's outcome becomes process state
fn exit_code(result: Result<(), CliError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Exit { code, message }) => {
            if let Some(message) = message {
                eprintln!("{message}");
            }
            ExitCode::from(code)
        }
        Err(CliError::Other(e)) => {
            eprintln!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run_mcp() -> anyhow::Result<()> {
    let path = store_path();
    let store = Store::open(&path).with_context(|| format!("open store at {}", path.display()))?;
    let store = Arc::new(Mutex::new(store));
    let context = ContextBuilder::new(Some(Arc::clone(&store))).build().await;
    let writer = StoreWriterConfig {
        store,
        author: author::resolve_author(None),
    };
    // The server's own session, as IssueAssign records it on each
    // assignment: the id Claude Code put in the environment, spelled
    // as a session URL by the default source. Without both the tool
    // is left out of the spec, since it could not name a holder.
    let session_url = match (
        env::var_os("CLAUDE_CODE_SESSION_ID"),
        source::open_source("", "gage mcp"),
    ) {
        (Some(id), Ok(opened)) => Some(opened.session_url(&id.to_string_lossy())),
        (None, _) => {
            tracing::warn!("IssueAssign unavailable: CLAUDE_CODE_SESSION_ID is not set");
            None
        }
        (_, Err(e)) => {
            let reason = match e {
                CliError::Exit { message, .. } => {
                    message.unwrap_or_else(|| "session source unavailable".to_string())
                }
                CliError::Other(err) => format!("{err:#}"),
            };
            tracing::warn!("IssueAssign unavailable: {reason}");
            None
        }
    };
    // IssuesFree asks whether a holder's session URL is live by
    // routing it through the driver registry. Any failure along the
    // way is an undetermined verdict with its reason.
    let session_live: SessionLiveFn = {
        let registry = source::driver_registry();
        Arc::new(move |url: &str| {
            let (source, id) = registry.split_session_url(url).map_err(|e| e.to_string())?;
            let opened = registry
                .open_source(&SourceOptions::new(&source))
                .map_err(|e| e.to_string())?;
            opened.is_session_live(&id).map_err(|e| e.to_string())
        })
    };
    // The names an interactive session may write notes under; scanner
    // output names stay with the scanners that declare them
    let note_names = BTreeMap::from([(
        "comment".to_string(),
        "A remark about a session or the work at hand".to_string(),
    )]);
    let spec = ToolSpec {
        gage: vec![
            GageTool::Query(QueryConfig {
                context: Arc::new(context),
            }),
            GageTool::IssueWrite(IssueWriteConfig::with_store(
                writer.clone(),
                "user-issue".to_string(),
                IssueStatus::Open,
            )),
            GageTool::NoteWrite(NoteWriteConfig::with_store(writer.clone(), note_names)),
            GageTool::IssueUpdate(writer.clone()),
            GageTool::IssueComment(writer.clone()),
            GageTool::IssuePendingResolve(writer.clone()),
            GageTool::IssuesFree(IssuesFreeConfig {
                writer: writer.clone(),
                session_live,
            }),
        ]
        .into_iter()
        .chain(session_url.map(|session_url| {
            GageTool::IssueAssign(IssueAssignConfig {
                writer,
                session_url,
            })
        }))
        .collect(),
        custom: Vec::new(),
    };
    serve_stdio(spec).await.map_err(|e| anyhow::anyhow!("{e}"))
}
