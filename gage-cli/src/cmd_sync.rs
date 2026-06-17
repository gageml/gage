use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{ArgGroup, Args};
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tabled::{
    Table,
    settings::{
        Color, Style,
        object::{Columns, Object, Rows},
    },
};
use tokio_util::sync::CancellationToken;

use gage_core::config::{Config, Remote, RemoteKind};
use gage_sync::{Observer, PullSource, PushTargets, SyncError};

use crate::style as cli_style;

#[derive(Args)]
#[command(group(
    ArgGroup::new("push_mode")
        .required(true)
        .args(["list_remotes", "remote", "all", "target"])
))]
pub struct PushArgs {
    /// List configured remotes
    #[arg(long = "list-remotes")]
    pub list_remotes: bool,

    /// Push to a named remote (repeatable)
    ///
    /// Selects one configured remote by name. Pass `-r` more than once
    /// to push to several. Mutually exclusive with `--all` and
    /// `--target`.
    #[arg(short = 'r', long = "remote", value_name = "NAME")]
    pub remote: Vec<String>,

    /// Push to every configured remote
    ///
    /// Mutually exclusive with `--remote` and `--target`.
    #[arg(short = 'a', long = "all", conflicts_with_all = ["remote", "target"])]
    pub all: bool,

    /// Push to a local directory
    ///
    /// Copies the payload directly into DIR (created if it does not
    /// exist), using the same layout as any other remote. Mutually
    /// exclusive with `--remote` and `--all`.
    #[arg(short = 't', long = "target", value_name = "DIR", conflicts_with_all = ["remote", "all"])]
    pub target: Option<PathBuf>,
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("pull_mode")
        .required(true)
        .args(["list_remotes", "remote", "source"])
))]
pub struct PullArgs {
    /// List configured remotes
    #[arg(long = "list-remotes")]
    pub list_remotes: bool,

    /// Pull from a named remote
    ///
    /// Selects one configured remote by name. Mutually exclusive with
    /// `--source`.
    #[arg(
        short = 'r',
        long = "remote",
        value_name = "NAME",
        conflicts_with = "source"
    )]
    pub remote: Option<String>,

    /// Pull from a local directory
    ///
    /// Reads the payload directly from DIR using the same layout as any
    /// other remote. Mutually exclusive with `--remote`.
    #[arg(short = 's', long = "source", value_name = "DIR")]
    pub source: Option<PathBuf>,

    /// Destination directory
    ///
    /// Where pulled files land locally. Defaults to `~/.gage-pull`. The
    /// remote tree lands under `<DIR>/gage/...` and `<DIR>/claude/...`,
    /// so existing files in DIR are not at risk of being overwritten.
    #[arg(short = 't', long = "target", value_name = "DIR")]
    pub target: Option<PathBuf>,
}

pub async fn push(args: PushArgs) {
    if args.list_remotes {
        list_remotes();
        return;
    }

    let targets = match resolve_push_targets(&args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("gage push: {e}");
            std::process::exit(1);
        }
    };

    let ui = Arc::new(SyncUi::new("Pushing", "Copying files to"));
    let observer: Arc<dyn Observer> = ui.clone();
    let cancel = install_ctrl_c();

    let result = gage_sync::push(targets, observer, cancel).await;
    ui.finish();
    exit_for(result, "gage push");
}

pub async fn pull(args: PullArgs) {
    if args.list_remotes {
        list_remotes();
        return;
    }

    let source = match resolve_pull_source(&args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gage pull: {e}");
            std::process::exit(1);
        }
    };

    let ui = Arc::new(SyncUi::new("Pulling", "Getting files from"));
    let observer: Arc<dyn Observer> = ui.clone();
    let cancel = install_ctrl_c();

    let result = gage_sync::pull(source, args.target.as_deref(), observer, cancel).await;
    ui.finish();
    exit_for(result, "gage pull");
}

fn resolve_push_targets(args: &PushArgs) -> Result<PushTargets, String> {
    if let Some(dir) = &args.target {
        return Ok(vec![ad_hoc_local_remote(dir)]);
    }
    let cfg = Config::load_user().map_err(|e| format!("loading config: {e}"))?;
    if cfg.remotes.is_empty() {
        return Err(
            "No remotes configured. Add `[[remote]]` entries to ~/.gage/config.toml".into(),
        );
    }
    if args.all {
        return Ok(cfg.remotes);
    }
    select_named(&cfg.remotes, &args.remote)
}

fn resolve_pull_source(args: &PullArgs) -> Result<PullSource, String> {
    if let Some(dir) = &args.source {
        return Ok(PullSource::Local(dir.clone()));
    }
    let name = args
        .remote
        .as_ref()
        .expect("ArgGroup guarantees one of remote/source/list_remotes");
    let cfg = Config::load_user().map_err(|e| format!("loading config: {e}"))?;
    if cfg.remotes.is_empty() {
        return Err(
            "No remotes configured. Add `[[remote]]` entries to ~/.gage/config.toml".into(),
        );
    }
    let remote = cfg
        .remotes
        .iter()
        .find(|r| &r.name == name)
        .ok_or_else(|| unknown_remote_message(name, &cfg.remotes))?;
    Ok(PullSource::Remote(remote.clone()))
}

fn select_named(remotes: &[Remote], names: &[String]) -> Result<Vec<Remote>, String> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let r = remotes
            .iter()
            .find(|r| &r.name == name)
            .ok_or_else(|| unknown_remote_message(name, remotes))?;
        out.push(r.clone());
    }
    Ok(out)
}

fn unknown_remote_message(name: &str, remotes: &[Remote]) -> String {
    let known = remotes
        .iter()
        .map(|r| r.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("unknown remote: {name}\n  configured remotes: {known}")
}

fn ad_hoc_local_remote(dir: &std::path::Path) -> Remote {
    Remote {
        name: format!("{}", dir.display()),
        kind: RemoteKind::Local {
            path: dir.to_path_buf(),
        },
    }
}

fn list_remotes() {
    let cfg = match Config::load_user() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gage: loading config: {e}");
            std::process::exit(1);
        }
    };
    if cfg.remotes.is_empty() {
        println!("No remotes configured");
        return;
    }

    let header = vec![
        "Name".to_string(),
        "Type".to_string(),
        "Location".to_string(),
    ];
    let rows: Vec<Vec<String>> = cfg
        .remotes
        .iter()
        .map(|r| {
            let (kind, loc) = match &r.kind {
                RemoteKind::Ssh { url } => ("ssh", url.clone()),
                RemoteKind::S3 { url, .. } => ("s3", url.clone()),
                RemoteKind::Local { path } => ("local", format!("{}", path.display())),
            };
            vec![r.name.clone(), kind.to_string(), loc]
        })
        .collect();

    let mut table = Table::from_iter(std::iter::once(header).chain(rows));
    table
        .with(Style::rounded())
        .modify(Rows::first(), Color::FG_BRIGHT_YELLOW)
        .modify(Columns::first().not(Rows::first()), Color::FG_BRIGHT_YELLOW)
        .modify(Columns::one(1).not(Rows::first()), cli_style::dim());
    println!("{table}");
}

fn exit_for(result: Result<(), SyncError>, prefix: &str) {
    match result {
        Ok(()) => {}
        Err(SyncError::Interrupted) => {
            eprintln!("{prefix}: interrupted");
            std::process::exit(130);
        }
        Err(e) => {
            eprintln!("{prefix}: {e}");
            std::process::exit(1);
        }
    }
}

/// Returns a token cancelled on the first SIGINT. A second SIGINT exits
/// the process immediately so an unresponsive backend can't trap the
/// user.
fn install_ctrl_c() -> CancellationToken {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        trigger.cancel();
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("interrupt: forcing exit");
            std::process::exit(130);
        }
    });
    cancel
}

/// Indicatif-based push/pull UI: a single spinner with status lines
/// printed above it. Pass-through of rsync's `-v` output and s3 per-file
/// notices preserves a useful log when stdout is captured.
struct SyncUi {
    multi: MultiProgress,
    spinner: ProgressBar,
    remote_phrase: &'static str,
}

impl SyncUi {
    fn new(initial: &str, remote_phrase: &'static str) -> Self {
        let multi = MultiProgress::new();
        let spinner = multi.add(ProgressBar::new_spinner());
        spinner.set_style(
            ProgressStyle::with_template("{spinner:.magenta} {msg} [{elapsed_precise}]").unwrap(),
        );
        spinner.enable_steady_tick(Duration::from_millis(120));
        spinner.set_message(format!("{initial}…"));
        Self {
            multi,
            spinner,
            remote_phrase,
        }
    }

    fn println(&self, line: &str) {
        #[allow(clippy::let_underscore_must_use)]
        let _ = self.multi.println(line);
    }

    fn finish(&self) {
        self.spinner.finish_and_clear();
        #[allow(clippy::let_underscore_must_use)]
        let _ = self.multi.clear();
    }
}

impl Observer for SyncUi {
    fn remote_start(&self, name: &str) {
        self.spinner
            .set_message(format!("{} {name}", self.remote_phrase));
        self.println(&style(format!("→ {name}")).cyan().to_string());
    }

    fn line(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.println(&format!("  {text}"));
    }

    fn remote_finish(&self, name: &str, result: Result<(), &str>) {
        match result {
            Ok(()) => {
                self.println(&style(format!("✓ {name}")).green().to_string());
            }
            Err(e) => {
                self.println(&style(format!("✗ {name}: {e}")).red().to_string());
            }
        }
    }
}
