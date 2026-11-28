use std::collections::BTreeMap;
use std::env;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use gage_core::config::default_cache_root;
use gage_mcp::{
    GageTool, IssueAssignConfig, IssueWriteConfig, IssuesFreeConfig, NoteWriteConfig,
    PublishConfig, QueryConfig, SessionLiveFn, SourceConfig, StoreWriterConfig, ToolSpec,
    serve_stdio,
};
use gage_publish::Staging;
use gage_query::ContextBuilder;
use gage_session::SourceOptions;
use gage_store::{IssueStatus, Store, store_path};
use humantime::DurationError;
use tokio_util::sync::CancellationToken;

use crate::error::CliError;
use crate::source::{SOURCE_HELP, STORED_REMOTE_HELP, stored_remote_is_read_only};

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
#[cfg(debug_assertions)]
mod cmd_dev;
mod cmd_fetch;
mod cmd_index;
mod cmd_init;
mod cmd_issue;
mod cmd_merge;
mod cmd_native;
mod cmd_note;
mod cmd_publish;
mod cmd_pull;
mod cmd_push;
mod cmd_query;
mod cmd_remote;
mod cmd_resolve;
mod cmd_scan;
mod cmd_session;
mod cmd_share;
mod cmd_status;
mod cmd_store;
mod cmd_tag;
mod dialog;
mod error;
mod filter_prompt;
#[cfg(test)]
mod help_lint;
mod human;
mod json;
mod limit;
mod markdown;
mod model_prompt;
mod progress;
mod scanner_attach;
mod scanner_spec;
mod session_filter;
mod session_identity;
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
    "gage_codex",
    "gage_core",
    "gage_corpus",
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
    /// Set up Gage in installed coding agents
    ///
    /// Creates the store when there is none and installs the Gage
    /// plugin, an MCP server and skills, in each coding agent Gage
    /// has a driver for. Run it again after upgrading Gage.
    Init(cmd_init::InitArgs),

    /// Run scanners on sessions
    Scan(cmd_scan::ScanArgs),

    /// Resolve issues in a coding agent session
    ///
    /// Starts an interactive session that resolves pending issues,
    /// then walks through open issues as directed.
    Resolve(cmd_resolve::ResolveArgs),

    /// Manage sessions
    Session {
        /// Session source
        #[arg(
            short = 'N',
            long,
            value_name = "SOURCE",
            long_help = format!("Session source\n\n{SOURCE_HELP}"),
            global = true,
            display_order = 1
        )]
        native: Option<String>,

        /// Operate on stored sessions
        #[arg(
            short = 'S',
            long,
            global = true,
            conflicts_with = "native",
            display_order = 2
        )]
        stored: bool,

        /// Operate on sessions fetched from remote
        #[arg(
            short = 'R',
            long,
            value_name = "NAME",
            long_help = format!("Operate on sessions fetched from remote\n\n{STORED_REMOTE_HELP}"),
            global = true,
            conflicts_with_all = ["native", "stored"],
            display_order = 3
        )]
        stored_remote: Option<String>,

        #[command(subcommand)]
        command: cmd_session::SessionCommand,
    },

    /// Publish a shareable copy of a session
    Publish(cmd_publish::PublishArgs),

    /// Manage datasets
    Dataset {
        #[command(subcommand)]
        command: cmd_dataset::DatasetCommand,
    },

    /// Manage shares
    ///
    /// A share is a set of published sessions to push to others,
    /// with the filters every member was published with.
    Share {
        #[command(subcommand)]
        command: cmd_share::ShareCommand,
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

    /// Manage native sources
    ///
    /// A native source is a directory of coding agent sessions. A
    /// name given to one is accepted wherever a session source is.
    Native {
        #[command(subcommand)]
        command: cmd_native::NativeCommand,
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

    /// Fetch objects from a remote
    ///
    /// Fetches the named objects and what they link, or the whole
    /// remote when none is named and the remote records no fetch
    /// default, and keeps them apart from the objects of the store.
    /// `gage merge` merges what was fetched.
    Fetch(cmd_fetch::FetchArgs),

    /// Merge fetched objects into store
    ///
    /// Merges the named objects and what they link, or everything
    /// fetched from the remote with --all. Reports what could not be
    /// merged. The remote is not contacted.
    Merge(cmd_merge::MergeArgs),

    /// Fetch and merge objects from a remote or URL
    ///
    /// Fetches the named objects and what they link, or the whole
    /// remote with --all, and merges them. Reports what could not be
    /// merged.
    Pull(cmd_pull::PullArgs),

    /// Back up store to a remote
    ///
    /// Adds every session from each configured source to the store,
    /// then sends every object, tag, and key to a remote added with
    /// `--backup`. Restoring is `gage pull`.
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

    /// Commands for developing Gage
    ///
    /// Commands for developing Gage. Present in debug builds only; a
    /// release build has no dev command
    #[cfg(debug_assertions)]
    Dev {
        #[command(subcommand)]
        command: cmd_dev::DevCommand,
    },
}

fn parse_duration(s: &str) -> Result<Duration, DurationError> {
    humantime::parse_duration(s)
}

/// The log layer and the progress indicator. Log lines go through the
/// indicator so they land above its row; progress spans go to the
/// indicator alone.
fn init_logging() {
    use tracing_subscriber::filter::{FilterExt, filter_fn};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Layer, fmt};

    let filter = EnvFilter::try_from_env("GAGE_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let stderr = fmt::layer()
        .with_writer(progress::LogWriter)
        .without_time()
        .with_filter(filter.and(filter_fn(|meta| {
            meta.target() != gage_session::progress::TARGET
        })));
    tracing_subscriber::registry()
        .with(stderr)
        .with(progress::layer())
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
    // `gage config` and `gage native` run without configured drivers
    // and without the project's store, so a config that names a
    // default driver this build does not have, a source that is not
    // one, or a store outside its project can still be edited
    if !matches!(cli.command, Command::Config { .. } | Command::Native { .. })
        && let Err(e) = source::configure_drivers().and_then(|()| source::configure_store())
    {
        return exit_code(Err(e));
    }
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
                cmd_tag::TagCommand::Rename(args) => cmd_tag::rename(args),
                cmd_tag::TagCommand::Delete(args) => cmd_tag::delete(args),
            },
            Command::Remote { command } => match command {
                cmd_remote::RemoteCommand::Add(args) => cmd_remote::add(args),
                cmd_remote::RemoteCommand::List => cmd_remote::list(),
                cmd_remote::RemoteCommand::Edit(args) => cmd_remote::edit(args),
                cmd_remote::RemoteCommand::Rename(args) => cmd_remote::rename(args),
                cmd_remote::RemoteCommand::Delete(args) => cmd_remote::delete(args),
            },
            Command::Native { command } => match command {
                cmd_native::NativeCommand::Add(args) => cmd_native::add(args),
                cmd_native::NativeCommand::List => cmd_native::list(),
                cmd_native::NativeCommand::Edit(args) => cmd_native::edit(args),
                cmd_native::NativeCommand::Rename(args) => cmd_native::rename(args),
                cmd_native::NativeCommand::Delete(args) => cmd_native::delete(args),
            },
            Command::Session {
                native: source,
                stored,
                stored_remote,
                command,
            } => match command {
                cmd_session::SessionCommand::List(args) => {
                    cmd_session::list(source, stored, stored_remote, args).await
                }
                cmd_session::SessionCommand::View(args) => {
                    cmd_session::view(source, stored, stored_remote, args).await
                }
                cmd_session::SessionCommand::Add(_) if stored_remote.is_some() => {
                    Err(stored_remote_is_read_only("gage session add"))
                }
                cmd_session::SessionCommand::Delete(_) if stored_remote.is_some() => {
                    Err(stored_remote_is_read_only("gage session delete"))
                }
                cmd_session::SessionCommand::Move(_) if stored_remote.is_some() => {
                    Err(stored_remote_is_read_only("gage session move"))
                }
                cmd_session::SessionCommand::Add(args) => {
                    cmd_session::add(source, stored, args).await
                }
                cmd_session::SessionCommand::Delete(args) => {
                    cmd_session::delete(source, stored, args).await
                }
                cmd_session::SessionCommand::Move(args) => cmd_session::move_(source, stored, args),
                cmd_session::SessionCommand::Find(args) => cmd_session::find(args).await,
            },
            Command::Publish(args) => cmd_publish::run(args).await,
            Command::Dataset { command } => match command {
                cmd_dataset::DatasetCommand::Add(args) => cmd_dataset::add(args).await,
                cmd_dataset::DatasetCommand::Refresh(args) => cmd_dataset::refresh(args),
                cmd_dataset::DatasetCommand::List(args) => cmd_dataset::list(args).await,
                cmd_dataset::DatasetCommand::Show(args) => cmd_dataset::show(args).await,
                cmd_dataset::DatasetCommand::Delete(args) => cmd_dataset::delete(args),
                cmd_dataset::DatasetCommand::Link(args) => cmd_dataset::link(args),
                cmd_dataset::DatasetCommand::Unlink(args) => cmd_dataset::unlink(args),
            },
            Command::Share { command } => match command {
                cmd_share::ShareCommand::Add(args) => cmd_share::add(args),
                cmd_share::ShareCommand::List(args) => cmd_share::list(args).await,
                cmd_share::ShareCommand::Show(args) => cmd_share::show(args),
                cmd_share::ShareCommand::Edit(args) => cmd_share::edit(args),
                cmd_share::ShareCommand::Delete(args) => cmd_share::delete(args),
                cmd_share::ShareCommand::Link(args) => cmd_share::link(args),
                cmd_share::ShareCommand::Unlink(args) => cmd_share::unlink(args),
            },
            Command::Attachment { command } => match command {
                cmd_attachment::AttachmentCommand::Add(args) => cmd_attachment::add(args).await,
                cmd_attachment::AttachmentCommand::List(args) => cmd_attachment::list(args).await,
                cmd_attachment::AttachmentCommand::Show(args) => cmd_attachment::show(args),
                cmd_attachment::AttachmentCommand::Delete(args) => cmd_attachment::delete(args),
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
            #[cfg(debug_assertions)]
            Command::Dev { command } => cmd_dev::run(command),
            Command::Push(args) => cmd_push::main(args).await,
            Command::Fetch(args) => cmd_fetch::main(args),
            Command::Merge(args) => cmd_merge::main(args),
            Command::Pull(args) => cmd_pull::main(args),
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
        Err(CliError::Usage(e)) => {
            e.print().unwrap();
            ExitCode::from(u8::try_from(e.exit_code()).expect("clap exit codes are 0 and 2"))
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
    // issue_assign records the session a call comes from as the
    // holder. A harness may name that session in this process's
    // environment or only in each call, so the drivers are asked per
    // call.
    let current_session = session_identity::current_session_fn(Arc::new(source::driver_registry()));
    // issues_free asks whether a holder's session URL is live by
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
                publish: Some(PublishConfig {
                    store: Arc::clone(&writer.store),
                    sessions: {
                        let store = Arc::clone(&writer.store);
                        Arc::new(move || {
                            let store = store
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            Staging::from(&*store).external().map_err(|e| e.to_string())
                        })
                    },
                }),
                source: Some(SourceConfig {
                    store: store_path(),
                    cache_root: default_cache_root(),
                }),
                views: Vec::new(),
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
            GageTool::IssueAssign(IssueAssignConfig {
                writer,
                current_session,
            }),
        ],
        custom: Vec::new(),
    };
    serve_stdio(spec).await.map_err(|e| anyhow::anyhow!("{e}"))
}
