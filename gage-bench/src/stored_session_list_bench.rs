//! The `stored-session-list` bench: baseline for `gage session list
//! --stored` against the store-bound `session` table.
//!
//! One run against a temp Gage home holding a store populated with
//! synthetic sessions:
//!
//! 1. **Populate.** Add `sessions` synthetic sessions through
//!    `SessionStore::add`. Timed per add.
//! 2. **Cold scenario.** Delete the object index so `Store::open`
//!    rebuilds it from the refs, then run each measured query once.
//! 3. **Warm scenario.** Reopen the store against the built index and
//!    run each measured query `iterations` times.
//!
//! Every scenario measures the same three queries as the
//! `claude-session-list` bench, against the store:
//!
//! - `count` — `SELECT COUNT(*) FROM session`. Served by the index.
//! - `list_light` — `SELECT id, mtime FROM session ORDER BY mtime DESC
//!   LIMIT n`. Served by the index.
//! - `list_full` — adds `session_type`, `title`, `model`, `size`,
//!   `message_count`, which read the top `n` objects.
//!
//! Metrics are named `<scenario>.<query>`; `<scenario>.open` times
//! `Store::open`, which reconciles the index.

use std::path::Path;
use std::sync::{Arc, Mutex};

use gage_query2::ContextBuilder;
use gage_store::{INDEX_FILE, SessionStore, Store};
use indicatif::ProgressBar;
use serde::Serialize;

use crate::measure::{Size, Timings};
use crate::report::{self, Results};
use crate::session_list_bench::{RowCounts, dir_size, emit_row_counts, first_int64, time_query};
use crate::synth::{Generator, SyntheticDriver, SyntheticSession};

pub const BENCH_NAME: &str = "stored-session-list";

#[derive(Debug, Clone, Serialize)]
pub struct Params {
    /// Sessions to add to the store.
    pub sessions: usize,
    /// KiB of content per session.
    pub session_kb: usize,
    /// List `LIMIT` value for the timed queries.
    pub limit: usize,
    /// Warm-scenario iterations.
    pub iterations: usize,
    /// Generator seed.
    pub seed: u64,
}

pub fn run(
    params: &Params,
    stamp: &str,
    run_dir: &Path,
    progress: &ProgressBar,
) -> Result<Results, String> {
    let home = run_dir.join("gage-home");
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    // SAFETY: set_var is unsafe in edition 2024; the bench is
    // single-threaded before this point, and every later store open
    // reads GAGE_HOME from the ambient process env.
    unsafe {
        std::env::set_var("GAGE_HOME", &home);
    }
    let store_path = home.join("store.git");
    gage_store::init(&store_path).map_err(|e| e.to_string())?;
    // A store runs `git gc --auto` when a writing handle closes. Off
    // here so a background collection never runs under a timed read.
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&store_path)
        .args(["config", "gc.auto", "0"])
        .status()
        .map_err(|e| format!("git config gc.auto: {e}"))?;
    if !status.success() {
        return Err(format!("git config gc.auto: {status}"));
    }

    progress.set_length((params.sessions + params.iterations + 3) as u64);
    progress.set_message("populate");
    let mut timings = Timings::default();
    let mut counts = Vec::new();
    {
        let store = Store::open(&store_path).map_err(|e| e.to_string())?;
        populate(&store, params, &mut timings, progress)?;
    }

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;

    progress.set_message("cold");
    let index_path = home.join(INDEX_FILE);
    std::fs::remove_file(&index_path)
        .map_err(|e| format!("remove {}: {e}", index_path.display()))?;
    let store = timings
        .time("cold.open", || Store::open(&store_path))
        .map_err(|e| e.to_string())?;
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store)))).build();
    let cold_row_counts = run_queries("cold", &rt, &ctx, params.limit, &mut timings)?;
    emit_row_counts("cold", &cold_row_counts, &mut counts);
    drop(ctx);
    progress.inc(1);

    progress.set_message("warm");
    let store = timings
        .time("warm.open", || Store::open(&store_path))
        .map_err(|e| e.to_string())?;
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store)))).build();
    let mut warm_row_counts = RowCounts::default();
    for _ in 0..params.iterations {
        warm_row_counts = run_queries("warm", &rt, &ctx, params.limit, &mut timings)?;
        progress.inc(1);
    }
    emit_row_counts("warm", &warm_row_counts, &mut counts);
    drop(ctx);

    let sizes = vec![
        Size {
            name: "store.git".into(),
            bytes: dir_size(&store_path),
        },
        Size {
            name: "object index".into(),
            bytes: std::fs::metadata(&index_path).map(|m| m.len()).unwrap_or(0),
        },
    ];

    Ok(Results {
        bench: BENCH_NAME.to_string(),
        stamp: stamp.to_string(),
        code_version: report::code_version(),
        params: serde_json::to_value(params).map_err(|e| e.to_string())?,
        metrics: timings.summarize(),
        sizes,
        counts,
        fsck: Vec::new(),
    })
}

fn populate(
    store: &Store,
    params: &Params,
    timings: &mut Timings,
    progress: &ProgressBar,
) -> Result<(), String> {
    let mut generator = Generator::new(params.seed);
    let sessions = SessionStore::from(store);
    let driver = SyntheticDriver;
    for i in 0..params.sessions {
        let native_id = format!("session-{i:06}");
        let content = generator.session_content(params.session_kb * 1024);
        let mut session = SyntheticSession::new(&native_id, &content);
        timings
            .time("add", || sessions.add(&driver, &mut session))
            .map_err(|e| e.to_string())?;
        progress.inc(1);
    }
    Ok(())
}

fn run_queries(
    scenario: &str,
    rt: &tokio::runtime::Runtime,
    ctx: &datafusion::prelude::SessionContext,
    limit: usize,
    timings: &mut Timings,
) -> Result<RowCounts, String> {
    let count = time_query(
        &format!("{scenario}.count"),
        rt,
        ctx,
        "SELECT COUNT(*) FROM session",
        timings,
    )?;
    let count_total = first_int64(&count).unwrap_or(0).max(0) as u64;

    let light_sql = format!("SELECT id, mtime FROM session ORDER BY mtime DESC LIMIT {limit}");
    let light = time_query(
        &format!("{scenario}.list_light"),
        rt,
        ctx,
        &light_sql,
        timings,
    )?;
    let list_light_rows: u64 = light.iter().map(|b| b.num_rows() as u64).sum();

    let full_sql = format!(
        "SELECT id, session_type, title, model, size, message_count, mtime \
         FROM session ORDER BY mtime DESC LIMIT {limit}"
    );
    let full = time_query(
        &format!("{scenario}.list_full"),
        rt,
        ctx,
        &full_sql,
        timings,
    )?;
    let list_full_rows: u64 = full.iter().map(|b| b.num_rows() as u64).sum();

    Ok(RowCounts {
        count_total,
        list_light_rows,
        list_full_rows,
    })
}
