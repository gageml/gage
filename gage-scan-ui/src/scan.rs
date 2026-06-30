//! Shared scan setup. Mirrors what `gage-cli`'s `cmd_scan::run_scan`
//! does, minus the interactive dialog — everything required must come
//! in on the command line.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use gage_claude::session::{self, SessionInfo};
use gage_db::db;
use gage_query::ScanSessionContext;
use gage_registry::scanner::{Scanner, ScannerRegistry};
use gage_scan::event::ScanEvent;

use crate::sink::UiEvent;

pub struct SetupArgs {
    pub sessions: Vec<String>,
    pub scanners: Vec<String>,
    pub files: Vec<String>,
    pub limit: Option<usize>,
    pub jobs: Option<usize>,
}

pub struct Setup {
    pub registry: ScannerRegistry,
    pub scanner_specs: Vec<String>,
    pub sessions: Vec<(String, PathBuf)>,
    pub jobs: usize,
}

pub struct RunSummary {
    pub scan_id: String,
    pub completed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub elapsed_secs: f64,
}

impl Setup {
    pub async fn resolve(args: SetupArgs) -> Result<Self> {
        let mut registry = ScannerRegistry::load();

        let mut file_specs: Vec<String> = Vec::new();
        for raw in &args.files {
            let (path_str, override_suffix) = match raw.find("#{") {
                Some(pos) => (&raw[..pos], &raw[pos..]),
                None => (raw.as_str(), ""),
            };
            let name = registry
                .register_file(std::path::Path::new(path_str))
                .map_err(|e| anyhow!("{e}"))?;
            let spec = format!("{name}{override_suffix}");
            if !file_specs.contains(&spec) {
                file_specs.push(spec);
            }
        }
        let mut scanner_specs = args.scanners.clone();
        scanner_specs.extend(file_specs);

        if scanner_specs.is_empty() {
            let cwd = std::env::current_dir().context("reading cwd")?;
            let (config, _) = gage_core::config::load_merged(&cwd)
                .with_context(|| format!("loading config from {}", cwd.display()))?;
            scanner_specs = registry
                .list_enabled(&config)
                .into_iter()
                .map(|d| d.name.clone())
                .collect();
        }

        for name in &scanner_specs {
            let bare = name.split("#{").next().unwrap();
            if !registry.is_known(bare) {
                return Err(anyhow!("unknown scanner: {bare}"));
            }
        }

        let sessions: Vec<(String, PathBuf)> = if args.sessions.is_empty() {
            let mut all = session::ls_sessions();
            if let Some(n) = args.limit {
                all.truncate(n);
            }
            all
        } else {
            let mut out = Vec::new();
            for prefix in &args.sessions {
                let s = session::one_session(prefix).map_err(|e| anyhow!("{e}"))?;
                out.push((s.id, s.src));
            }
            out
        };

        if sessions.is_empty() {
            return Err(anyhow!("no sessions selected"));
        }

        let jobs = args.jobs.unwrap_or_else(num_cpus::get).max(1);

        Ok(Setup {
            registry,
            scanner_specs,
            sessions,
            jobs,
        })
    }
}

/// Run the scan, forwarding adapted `UiEvent`s to `tx`. Returns when
/// the runner returns. The caller is responsible for draining `tx`
/// before reading the summary if it wants every event processed.
pub async fn run_scan(setup: Setup, tx: mpsc::UnboundedSender<UiEvent>) -> Result<RunSummary> {
    let scanners: Vec<Scanner<'_>> = {
        let mut out = Vec::new();
        for spec in &setup.scanner_specs {
            let s = Scanner::from_spec(spec, &setup.registry).map_err(|e| anyhow!("{e}"))?;
            out.push(s);
        }
        out
    };

    let selected: Arc<[SessionInfo]> = {
        let mut out: Vec<SessionInfo> = Vec::with_capacity(setup.sessions.len());
        for (id, src) in setup.sessions {
            let meta = std::fs::metadata(&src)
                .with_context(|| format!("stat session file {}", src.display()))?;
            let mtime = meta.modified().unwrap();
            out.push(SessionInfo {
                id,
                src,
                mtime,
                size: meta.len(),
            });
        }
        Arc::from(out.into_boxed_slice())
    };
    let scan_ctx = Arc::new(ScanSessionContext::new(&selected));

    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        });
    }

    let db = Arc::new(Mutex::new(db::open_db().map_err(|e| anyhow!("{e}"))?));

    let started = Instant::now();

    let mut print_buf = String::new();
    let result = gage_scan::runner::run(
        db,
        scanners,
        selected,
        scan_ctx,
        setup.jobs,
        4,
        cancel,
        |event| match event {
            ScanEvent::Status(s) => {
                let _ = tx.send(UiEvent::Status(s));
            }
            ScanEvent::Print { s } => {
                let bytes = s.len() as u64;
                print_buf.push_str(&s);
                let _ = tx.send(UiEvent::Bytes(bytes));
                while let Some(i) = print_buf.find('\n') {
                    let line: String = print_buf.drain(..=i).collect();
                    let _ = tx.send(UiEvent::Log(line.trim_end_matches('\n').to_string()));
                }
            }
            ScanEvent::Println { s } => {
                let bytes = s.len() as u64;
                let _ = tx.send(UiEvent::Bytes(bytes));
                let _ = tx.send(UiEvent::Log(s));
            }
            ScanEvent::TaskFailed {
                scanner,
                task,
                message,
            } => {
                let _ = tx.send(UiEvent::Failed {
                    scanner,
                    task,
                    message,
                });
            }
            ScanEvent::Warning {
                scanner,
                task,
                message,
            } => {
                let _ = tx.send(UiEvent::Warning {
                    scanner,
                    task,
                    message,
                });
            }
        },
    )
    .await;

    if !print_buf.is_empty() {
        let _ = tx.send(UiEvent::Log(std::mem::take(&mut print_buf)));
    }
    let _ = tx.send(UiEvent::Finished);

    let elapsed_secs = started.elapsed().as_secs_f64();
    match result {
        Ok(s) => Ok(RunSummary {
            scan_id: s.scan_id,
            completed: s.completed,
            failed: s.failed,
            skipped: s.skipped,
            elapsed_secs,
        }),
        Err(e) => Err(anyhow!("{e}")),
    }
}
