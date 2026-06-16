use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio_util::sync::CancellationToken;

use gage_sync::{Observer, SyncError};

#[derive(Args)]
pub struct PushArgs {}

#[derive(Args)]
pub struct PullArgs {
    /// Name of the remote to pull from. Required when more than one
    /// remote is configured.
    #[arg(value_name = "REMOTE")]
    pub remote: Option<String>,

    /// Target directory. Defaults to `~/.gage-pull`. The remote tree
    /// lands under `<dir>/gage/...` and `<dir>/claude/...`, so existing
    /// files in `<dir>` are not at risk of being overwritten
    #[arg(long = "into", short = 't', value_name = "DIR")]
    pub into: Option<PathBuf>,
}

pub async fn push(_args: PushArgs) {
    let ui = Arc::new(SyncUi::new("Pushing", "Copying files to"));
    let observer: Arc<dyn Observer> = ui.clone();
    let cancel = install_ctrl_c();

    let result = gage_sync::push(observer, cancel).await;
    ui.finish();
    exit_for(result, "gage push");
}

pub async fn pull(args: PullArgs) {
    let ui = Arc::new(SyncUi::new("Pulling", "Getting files from"));
    let observer: Arc<dyn Observer> = ui.clone();
    let cancel = install_ctrl_c();

    let result = gage_sync::pull(
        args.remote.as_deref(),
        args.into.as_deref(),
        observer,
        cancel,
    )
    .await;
    ui.finish();
    exit_for(result, "gage pull");
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
