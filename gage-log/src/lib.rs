//! Process logging for long-running gage commands.
//!
//! Each entry point that wants logs calls [`init`] once with its role name
//! (`"mcp"`, `"scan"`, `"lsp"`). Logs are written to `~/.gage/log/` in
//! per-process files named `<role>.<pid>.<date>`. On init a background
//! thread sweeps the directory: closed files (any non-`.gz` file whose
//! date is before today) are gzipped, and anything with mtime older than
//! 30 days is removed.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, SystemTime};

use chrono::Utc;
use flate2::Compression;
use flate2::write::GzEncoder;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

struct PanicTarget {
    dir: PathBuf,
    prefix: String,
}

static PANIC_TARGET: OnceLock<PanicTarget> = OnceLock::new();

/// Installs the tracing subscriber for `role` and spawns a one-shot sweep
/// of `~/.gage/log/`. The returned guard flushes the non-blocking writer
/// on drop; the caller must hold it for the process lifetime.
pub fn init(role: &str) -> io::Result<WorkerGuard> {
    let log_dir = gage_core::config::gage_home().join("log");
    fs::create_dir_all(&log_dir)?;

    let prefix = format!("{role}.{}", std::process::id());
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(&prefix)
        .build(&log_dir)
        .map_err(io::Error::other)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_env("GAGE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .init();

    install_panic_hook(log_dir.clone(), prefix);

    let dir = log_dir;
    thread::spawn(move || {
        if let Err(e) = sweep(&dir) {
            tracing::warn!("log sweep failed: {e}");
        }
    });

    Ok(guard)
}

/// Chains a panic hook that writes the panic synchronously to today's
/// log file before invoking whatever hook was previously installed.
/// Synchronous because `panic = "abort"` (release profile) skips drops,
/// which means the non-blocking tracing writer never flushes.
fn install_panic_hook(dir: PathBuf, prefix: String) {
    if PANIC_TARGET.set(PanicTarget { dir, prefix }).is_err() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_panic(info);
        previous(info);
    }));
}

fn write_panic(info: &std::panic::PanicHookInfo<'_>) {
    let Some(target) = PANIC_TARGET.get() else {
        return;
    };
    let today = Utc::now().format("%Y-%m-%d");
    let path = target.dir.join(format!("{}.{today}", target.prefix));
    let backtrace = std::backtrace::Backtrace::force_capture();
    let now = Utc::now().to_rfc3339();
    let msg = format!("{now} PANIC {info}\n{backtrace}\n");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path)
        && let Err(e) = f.write_all(msg.as_bytes())
    {
        eprintln!("gage-log: failed to write panic: {e}");
    }
}

fn sweep(dir: &Path) -> io::Result<()> {
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let cutoff = SystemTime::now()
        .checked_sub(RETENTION)
        .unwrap_or(SystemTime::UNIX_EPOCH);

    for entry in fs::read_dir(dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("log sweep: read_dir entry: {e}");
                continue;
            }
        };
        if let Err(e) = sweep_entry(&entry, &today, cutoff) {
            tracing::warn!("log sweep {}: {}", entry.path().display(), e);
        }
    }
    Ok(())
}

fn sweep_entry(entry: &fs::DirEntry, today: &str, cutoff: SystemTime) -> io::Result<()> {
    let path = entry.path();
    let meta = entry.metadata()?;
    if !meta.is_file() {
        return Ok(());
    }

    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    let Some(parsed) = parse_log_name(name) else {
        return Ok(());
    };

    if let Ok(mtime) = meta.modified()
        && mtime < cutoff
    {
        return remove_if_present(&path);
    }

    if parsed.compressed || parsed.tmp || parsed.date == today {
        return Ok(());
    }

    gzip_in_place(&path)
}

struct ParsedName<'a> {
    date: &'a str,
    compressed: bool,
    tmp: bool,
}

/// Matches `<role>.<pid>.<YYYY-MM-DD>[.gz|.gz.tmp]` where `role` is
/// lowercase ASCII, `pid` is digits, and `date` is `\d{4}-\d{2}-\d{2}`.
/// Anything else (e.g. `slow.db`, `slow.db-wal`) returns `None` so the
/// sweep leaves it alone.
fn parse_log_name(name: &str) -> Option<ParsedName<'_>> {
    let (stem, compressed, tmp) = if let Some(s) = name.strip_suffix(".gz.tmp") {
        (s, true, true)
    } else if let Some(s) = name.strip_suffix(".gz") {
        (s, true, false)
    } else {
        (name, false, false)
    };
    let mut parts = stem.splitn(3, '.');
    let role = parts.next()?;
    let pid = parts.next()?;
    let date = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if role.is_empty() || !role.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !is_iso_date(date) {
        return None;
    }
    Some(ParsedName {
        date,
        compressed,
        tmp,
    })
}

fn is_iso_date(s: &str) -> bool {
    if s.len() != 10 {
        return false;
    }
    s.bytes()
        .zip(b"DDDD-DD-DD".iter())
        .all(|(c, pat)| match pat {
            b'D' => c.is_ascii_digit(),
            b'-' => c == b'-',
            _ => false,
        })
}

fn gzip_in_place(path: &Path) -> io::Result<()> {
    let tmp = with_suffix(path, ".gz.tmp");
    let final_path = with_suffix(path, ".gz");

    {
        let mut src = fs::File::open(path)?;
        let dst = fs::File::create(&tmp)?;
        let mut enc = GzEncoder::new(dst, Compression::default());
        io::copy(&mut src, &mut enc)?;
        let f = enc.finish()?;
        f.sync_all()?;
    }

    fs::rename(&tmp, &final_path)?;
    remove_if_present(path)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_log_name;

    #[test]
    fn matches_active_log() {
        let p = parse_log_name("mcp.12345.2026-06-22").unwrap();
        assert_eq!(p.date, "2026-06-22");
        assert!(!p.compressed);
        assert!(!p.tmp);
    }

    #[test]
    fn matches_compressed_and_tmp() {
        assert!(parse_log_name("scan.1.2026-06-22.gz").unwrap().compressed);
        let p = parse_log_name("lsp.99.2026-06-22.gz.tmp").unwrap();
        assert!(p.compressed && p.tmp);
    }

    #[test]
    fn rejects_unrelated_files() {
        assert!(parse_log_name("slow.db").is_none());
        assert!(parse_log_name("slow.db-wal").is_none());
        assert!(parse_log_name("slow.db-shm").is_none());
        assert!(parse_log_name("mcp.12345").is_none());
        assert!(parse_log_name("mcp.abc.2026-06-22").is_none());
        assert!(parse_log_name("MCP.12345.2026-06-22").is_none());
        assert!(parse_log_name("mcp.12345.2026-6-22").is_none());
        assert!(parse_log_name("mcp.12345.2026-06-22.extra").is_none());
    }
}
