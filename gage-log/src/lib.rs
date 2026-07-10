//! Process logging for long-running gage commands.
//!
//! Logs live under `~/.gage/log/{role}/` — one subdirectory per role
//! (`mcp`, `scan`, `lsp`). Entry points without a domain identity call
//! [`init`], which names the file `{timestamp}-{pid}.log` (timestamp
//! first so lexical order is recency order). Entry points keyed by a
//! domain id — a scan run — call [`init_named`] with that id, giving
//! `{scan_id}.log`; sibling streams (`{scan_id}.out`, `{scan_id}.err`)
//! are written by the scan command next to it.
//!
//! The file is created lazily on the first log write, so a process that
//! logs nothing leaves no file behind. On init a background thread
//! sweeps the role directories: closed files are gzipped (a file is
//! closed when its `-{pid}` suffix names a dead process, or — for
//! id-named files with no pid — when it has been quiet for a day), and
//! anything older than 30 days is removed. Files from the legacy flat
//! `~/.gage/log/` layout are swept by the old rules until they age out.

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
use tracing_subscriber::EnvFilter;

const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Quiet period after which an id-named file (no embedded pid) counts
/// as closed and is compressed by the sweep.
const QUIET: Duration = Duration::from_secs(24 * 60 * 60);

static PANIC_TARGET: OnceLock<PathBuf> = OnceLock::new();

/// The log directory for a role, `~/.gage/log/{role}`.
pub fn role_dir(role: &str) -> PathBuf {
    gage_core::config::gage_home().join("log").join(role)
}

/// Installs the tracing subscriber for `role`, logging to
/// `~/.gage/log/{role}/{timestamp}-{pid}.log`. The returned guard
/// flushes the non-blocking writer on drop; the caller must hold it
/// for the process lifetime.
pub fn init(role: &str) -> io::Result<WorkerGuard> {
    let ts = Utc::now().format("%Y-%m-%dT%H-%M-%S");
    let name = format!("{ts}-{}", std::process::id());
    init_named(role, &name)
}

/// Installs the tracing subscriber for `role`, logging to
/// `~/.gage/log/{role}/{name}.log`. Use when the process has a domain
/// identity (e.g. a scan id) that should key its logs.
pub fn init_named(role: &str, name: &str) -> io::Result<WorkerGuard> {
    let dir = role_dir(role);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.log"));
    let (writer, guard) = tracing_appender::non_blocking(LazyFile::new(path.clone()));

    let filter = EnvFilter::try_from_env("GAGE_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .init();

    install_panic_hook(path);

    let log_root = gage_core::config::gage_home().join("log");
    thread::spawn(move || {
        if let Err(e) = sweep(&log_root) {
            tracing::warn!("log sweep failed: {e}");
        }
    });

    Ok(guard)
}

/// File writer that defers creating the log file until the first write,
/// so a process that never logs leaves no empty file behind.
struct LazyFile {
    path: PathBuf,
    file: Option<fs::File>,
}

impl LazyFile {
    fn new(path: PathBuf) -> Self {
        Self { path, file: None }
    }
}

impl Write for LazyFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.file.is_none() {
            let f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            self.file = Some(f);
        }
        self.file.as_mut().unwrap().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// Chains a panic hook that writes the panic synchronously to this
/// process's log file before invoking whatever hook was previously
/// installed. Synchronous because `panic = "abort"` (release profile)
/// skips drops, which means the non-blocking tracing writer never
/// flushes.
fn install_panic_hook(path: PathBuf) {
    if PANIC_TARGET.set(path).is_err() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_panic(info);
        previous(info);
    }));
}

fn write_panic(info: &std::panic::PanicHookInfo<'_>) {
    let Some(path) = PANIC_TARGET.get() else {
        return;
    };
    let backtrace = std::backtrace::Backtrace::force_capture();
    let now = Utc::now().to_rfc3339();
    let msg = format!("{now} PANIC {info}\n{backtrace}\n");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path)
        && let Err(e) = f.write_all(msg.as_bytes())
    {
        eprintln!("gage-log: failed to write panic: {e}");
    }
}

/// Sweep the log root: role subdirectories by the current rules, files
/// in the root itself by the legacy flat-layout rules.
fn sweep(root: &Path) -> io::Result<()> {
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let now = SystemTime::now();
    let cutoff = now.checked_sub(RETENTION).unwrap_or(SystemTime::UNIX_EPOCH);
    let quiet_cutoff = now.checked_sub(QUIET).unwrap_or(SystemTime::UNIX_EPOCH);

    for entry in fs::read_dir(root)? {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("log sweep: read_dir entry: {e}");
                continue;
            }
        };
        let path = entry.path();
        let result = if path.is_dir() {
            sweep_role_dir(&path, cutoff, quiet_cutoff)
        } else {
            sweep_legacy_entry(&entry, &today, cutoff)
        };
        if let Err(e) = result {
            tracing::warn!("log sweep {}: {}", path.display(), e);
        }
    }
    Ok(())
}

fn sweep_role_dir(dir: &Path, cutoff: SystemTime, quiet_cutoff: SystemTime) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Err(e) = sweep_role_entry(&entry, cutoff, quiet_cutoff) {
            tracing::warn!("log sweep {}: {}", entry.path().display(), e);
        }
    }
    Ok(())
}

fn sweep_role_entry(
    entry: &fs::DirEntry,
    cutoff: SystemTime,
    quiet_cutoff: SystemTime,
) -> io::Result<()> {
    let path = entry.path();
    let meta = entry.metadata()?;
    if !meta.is_file() {
        return Ok(());
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    let Some(parsed) = parse_role_log_name(name) else {
        return Ok(());
    };

    let mtime = meta.modified()?;
    if mtime < cutoff {
        return remove_if_present(&path);
    }
    if parsed.compressed {
        return Ok(());
    }

    // A file is closed — safe to compress — when its owning process is
    // gone. Pid-suffixed names are checked directly; id-named files
    // (e.g. a scan id) count as closed after a quiet period.
    let closed = match parsed.pid {
        Some(pid) => !pid_alive(pid),
        None => mtime < quiet_cutoff,
    };
    if closed {
        gzip_in_place(&path)?;
    }
    Ok(())
}

struct RoleLogName<'a> {
    /// Trailing `-{pid}` in the stem, when present
    pid: Option<&'a str>,
    compressed: bool,
}

/// Matches `{stem}.{log|out|err}[.gz[.tmp]]`. Anything else is left
/// alone.
fn parse_role_log_name(name: &str) -> Option<RoleLogName<'_>> {
    let (rest, compressed) = if let Some(s) = name.strip_suffix(".gz.tmp") {
        (s, true)
    } else if let Some(s) = name.strip_suffix(".gz") {
        (s, true)
    } else {
        (name, false)
    };
    let (stem, ext) = rest.rsplit_once('.')?;
    if !matches!(ext, "log" | "out" | "err") || stem.is_empty() {
        return None;
    }
    // The `-{pid}` suffix is only trusted on `{timestamp}-{pid}` names
    // ([`init`]'s scheme). An id-named stem (e.g. a uuid) could end in
    // an all-digit segment by chance, and misreading it as a dead pid
    // would compress a live file out from under its writer.
    let pid = has_timestamp_prefix(stem)
        .then(|| stem.rsplit_once('-').map(|(_, p)| p))
        .flatten()
        .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    Some(RoleLogName { pid, compressed })
}

/// True when the name starts with `init`'s `%Y-%m-%dT` timestamp shape.
fn has_timestamp_prefix(stem: &str) -> bool {
    matches!(stem.get(..10), Some(date) if is_iso_date(date))
        && stem.as_bytes().get(10) == Some(&b'T')
}

fn sweep_legacy_entry(entry: &fs::DirEntry, today: &str, cutoff: SystemTime) -> io::Result<()> {
    let path = entry.path();
    let meta = entry.metadata()?;
    if !meta.is_file() {
        return Ok(());
    }

    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    let Some(parsed) = parse_legacy_log_name(name) else {
        return Ok(());
    };

    // An uncompressed file whose process is still running is an active
    // log (possibly spanning midnight); gzipping or removing it would
    // orphan the writer's open handle and lose subsequent writes.
    if !parsed.compressed && pid_alive(parsed.pid) {
        return Ok(());
    }

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

/// True when a process with this pid is currently running. Uses `/proc`,
/// so on non-Linux hosts this returns false and the sweep behaves as if
/// the process has exited.
fn pid_alive(pid: &str) -> bool {
    Path::new("/proc").join(pid).is_dir()
}

struct LegacyLogName<'a> {
    pid: &'a str,
    date: &'a str,
    compressed: bool,
    tmp: bool,
}

/// Matches the legacy flat-layout name `<role>.<pid>.<YYYY-MM-DD>[.gz|
/// .gz.tmp]` where `role` is lowercase ASCII, `pid` is digits, and
/// `date` is `\d{4}-\d{2}-\d{2}`. Anything else (e.g. `slow.db`,
/// `slow.db-wal`) returns `None` so the sweep leaves it alone.
fn parse_legacy_log_name(name: &str) -> Option<LegacyLogName<'_>> {
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
    Some(LegacyLogName {
        pid,
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
    use super::{parse_legacy_log_name, parse_role_log_name};

    #[test]
    fn legacy_matches_active_log() {
        let p = parse_legacy_log_name("mcp.12345.2026-06-22").unwrap();
        assert_eq!(p.pid, "12345");
        assert_eq!(p.date, "2026-06-22");
        assert!(!p.compressed);
        assert!(!p.tmp);
    }

    #[test]
    fn legacy_matches_compressed_and_tmp() {
        assert!(
            parse_legacy_log_name("scan.1.2026-06-22.gz")
                .unwrap()
                .compressed
        );
        let p = parse_legacy_log_name("lsp.99.2026-06-22.gz.tmp").unwrap();
        assert!(p.compressed && p.tmp);
    }

    #[test]
    fn legacy_rejects_unrelated_files() {
        assert!(parse_legacy_log_name("slow.db").is_none());
        assert!(parse_legacy_log_name("slow.db-wal").is_none());
        assert!(parse_legacy_log_name("slow.db-shm").is_none());
        assert!(parse_legacy_log_name("mcp.12345").is_none());
        assert!(parse_legacy_log_name("mcp.abc.2026-06-22").is_none());
        assert!(parse_legacy_log_name("MCP.12345.2026-06-22").is_none());
        assert!(parse_legacy_log_name("mcp.12345.2026-6-22").is_none());
        assert!(parse_legacy_log_name("mcp.12345.2026-06-22.extra").is_none());
    }

    #[test]
    fn role_matches_pid_suffixed_log() {
        let p = parse_role_log_name("2026-07-08T14-19-41-4471.log").unwrap();
        assert_eq!(p.pid, Some("4471"));
        assert!(!p.compressed);
    }

    #[test]
    fn role_matches_id_named_streams() {
        for name in ["6ezfkac4.log", "6ezfkac4.out", "6ezfkac4.err"] {
            let p = parse_role_log_name(name).unwrap();
            assert_eq!(p.pid, None);
            assert!(!p.compressed);
        }
        assert!(parse_role_log_name("6ezfkac4.out.gz").unwrap().compressed);
    }

    #[test]
    fn role_pid_requires_timestamp_prefix() {
        // A uuid stem can end in an all-digit segment by chance; only
        // timestamp-prefixed names carry a trusted pid
        let p = parse_role_log_name("0f7a38dc-1a72-235711.log").unwrap();
        assert_eq!(p.pid, None);
        let p = parse_role_log_name("0f7a38dc-1a72-23a5.log").unwrap();
        assert_eq!(p.pid, None);
    }

    #[test]
    fn role_rejects_unrelated_files() {
        assert!(parse_role_log_name("notes.txt").is_none());
        assert!(parse_role_log_name("log").is_none());
    }
}
