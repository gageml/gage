use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::backend::{Backend, SyncError};
use crate::observer::Observer;
use crate::payload::TransferItem;

/// SSH/rsync backend. Shells out to the system `rsync` and `ssh`.
/// `url` is an rsync-style target, e.g. `gar1t.gageml.com:/srv/backup/gage`
/// or `user@host:/path/to/dir`.
pub struct SshBackend {
    url: String,
}

impl SshBackend {
    pub fn new(url: String) -> Self {
        Self {
            url: trim_trailing_slash(url),
        }
    }
}

fn trim_trailing_slash(mut s: String) -> String {
    while s.ends_with('/') {
        s.pop();
    }
    s
}

#[async_trait]
impl Backend for SshBackend {
    async fn put_all(
        &self,
        items: &[TransferItem],
        observer: Arc<dyn Observer>,
        cancel: CancellationToken,
    ) -> Result<(), SyncError> {
        let staging = TempDir::new()?;
        stage_items(staging.path(), items)?;

        let mut src = staging.path().to_string_lossy().into_owned();
        src.push('/');
        let mut dest = self.url.clone();
        dest.push('/');

        let mut cmd = Command::new("rsync");
        cmd.args(["-aLv", "--copy-unsafe-links"])
            .arg(&src)
            .arg(&dest)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_rsync(cmd, observer, cancel).await
    }

    async fn fetch_all(
        &self,
        local: &Path,
        observer: Arc<dyn Observer>,
        cancel: CancellationToken,
    ) -> Result<(), SyncError> {
        tokio::fs::create_dir_all(local).await?;

        let mut src = self.url.clone();
        src.push('/');
        let mut dest = local.to_string_lossy().into_owned();
        dest.push('/');

        let mut cmd = Command::new("rsync");
        cmd.arg("-av")
            .arg(&src)
            .arg(&dest)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_rsync(cmd, observer, cancel).await
    }
}

fn stage_items(stage: &Path, items: &[TransferItem]) -> Result<(), SyncError> {
    for item in items {
        let dest = stage.join(&item.remote);
        let parent = dest
            .parent()
            .expect("staged path always has a parent under the staging root");
        std::fs::create_dir_all(parent)?;
        let target = absolute(&item.local)?;
        symlink(&target, &dest)?;
    }
    Ok(())
}

fn absolute(p: &Path) -> Result<PathBuf, SyncError> {
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(p))
}

async fn run_rsync(
    mut cmd: Command,
    observer: Arc<dyn Observer>,
    cancel: CancellationToken,
) -> Result<(), SyncError> {
    let mut child = cmd.spawn().map_err(|e| {
        SyncError::Backend(format!("failed to spawn rsync: {e} (is `rsync` on PATH?)"))
    })?;

    let stdout = child
        .stdout
        .take()
        .expect("rsync child was spawned with piped stdout");
    let stderr = child
        .stderr
        .take()
        .expect("rsync child was spawned with piped stderr");

    let out_task = spawn_line_pump(stdout, Arc::clone(&observer));
    let err_task = spawn_line_pump(stderr, Arc::clone(&observer));

    let wait_result = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            // Terminal-driven Ctrl-C already SIGINTs the whole process
            // group, so rsync is on its way out; reap it.
            reap_child(&mut child).await;
            let _out = out_task.await;
            let _err = err_task.await;
            return Err(SyncError::Interrupted);
        }
        res = child.wait() => res,
    };

    let _out = out_task.await;
    let _err = err_task.await;

    let status = wait_result?;
    if !status.success() {
        return Err(SyncError::Backend(format!("rsync exited with {status}")));
    }
    Ok(())
}

async fn reap_child(child: &mut Child) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    if (tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await).is_ok() {
        return;
    }
    // Last resort: rsync didn't exit on SIGINT; force-kill.
    #[allow(clippy::let_underscore_must_use)]
    let _ = child.start_kill();
    #[allow(clippy::let_underscore_must_use)]
    let _ = child.wait().await;
}

fn spawn_line_pump<R>(reader: R, observer: Arc<dyn Observer>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => observer.line(&line),
                Ok(None) => break,
                Err(_) => break,
            }
        }
    })
}
