//! The `claude-session-list` bench: baseline for `gage session list`
//! performance against the Claude driver, going through the SQL
//! surface in gage-query.
//!
//! One run against a temp directory holding synthetic Claude sessions:
//!
//! 1. **Populate.** Generate `sessions` synthetic JSONL files spread
//!    across `projects` project slugs. Not timed.
//! 2. **Cold scenario.** One iteration of each measured query with
//!    both the on-disk summary cache and the process-local session
//!    cache empty.
//! 3. **Reconcile.** Fill the summary cache once. Timed on its own line.
//! 4. **Warm scenario.** `iterations` runs of each measured query
//!    against the warmed cache.
//!
//! Every scenario measures three queries:
//!
//! - `count` — `SELECT COUNT(*) FROM session`. No summary columns
//!   touched; times the enumeration cost.
//! - `list_light` — `SELECT id, project, mtime FROM session ORDER BY
//!   mtime DESC LIMIT n`. Enumeration + sort + limit; still no
//!   summary.
//! - `list_full` — `SELECT id, project, title, model, size,
//!   message_count, mtime FROM session ORDER BY mtime DESC LIMIT n`.
//!   Adds the summary columns, so `SessionExec` fetches the summary
//!   for the top `n` sessions (cache lookup on warm, derive on cold).
//!
//! Metrics are named `<scenario>.<query>`. This bench does not compare
//! against the pre-refactor bench of the same name; the two measure
//! fundamentally different code paths.

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use arrow::array::{Array, Int64Array};
use datafusion::error::DataFusionError;
use datafusion::prelude::SessionContext;
use gage_claude::index::{IndexStore, LockMode, cache_dir_for};
use gage_registry::driver::DriverRegistry;
use indicatif::ProgressBar;
use rand::{Rng, SeedableRng, rngs::StdRng};
use serde::Serialize;

use crate::measure::{Count, Size, Timings};
use crate::report::{self, Results};
use crate::synth::Generator;

pub const BENCH_NAME: &str = "claude-session-list";

#[derive(Debug, Clone, Serialize)]
pub struct Params {
    /// Total session files to generate under `projects/`.
    pub sessions: usize,
    /// Number of project slugs the sessions are distributed across.
    pub projects: usize,
    /// KiB of JSONL content per session file.
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
    let projects_dir = run_dir.join("projects");
    let gage_home = run_dir.join("gage-home");
    std::fs::create_dir_all(&projects_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&gage_home).map_err(|e| e.to_string())?;
    // SAFETY: set_var is unsafe in edition 2024; the bench is
    // single-threaded before this point, and every subsequent
    // spawn_blocking / reconcile / DataFusion call reads these vars
    // from the ambient process env.
    unsafe {
        std::env::set_var("GAGE_HOME", &gage_home);
        std::env::set_var("CLAUDE_CONFIG_DIR", run_dir);
    }

    // The cache lives where the driver keys it, under GAGE_HOME
    let cache_dir = cache_dir_for(&projects_dir);
    let registry = DriverRegistry::builtin();

    progress.set_length((params.sessions + params.iterations + 4) as u64);
    progress.set_message("populate");
    populate(&projects_dir, params, progress).map_err(|e| e.to_string())?;

    let mut timings = Timings::default();
    let mut counts = Vec::new();

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;

    progress.set_message("cold");
    let source = registry.open_source("").map_err(|e| e.to_string())?;
    let ctx = rt
        .block_on(gage_query::create_context(source.as_ref()))
        .map_err(|e| e.to_string())?;
    let cold_row_counts = run_queries("cold", &rt, &ctx, params.limit, &mut timings)?;
    emit_row_counts("cold", &cold_row_counts, &mut counts);
    // Drop and recreate for reconcile+warm so nothing carries in the
    // in-memory session cache. Reconcile fills the on-disk cache.
    drop(ctx);
    progress.inc(1);

    progress.set_message("reconcile");
    let index = IndexStore::new(&projects_dir, &cache_dir);
    let start = Instant::now();
    let outcome = index.reconcile(LockMode::Wait).map_err(|e| e.to_string())?;
    timings.record("reconcile", start.elapsed().as_secs_f64() * 1000.0);
    counts.push(Count {
        name: "reconcile.discovered".into(),
        value: outcome.discovered as u64,
    });
    counts.push(Count {
        name: "reconcile.indexed".into(),
        value: outcome.indexed as u64,
    });
    progress.inc(1);

    progress.set_message("warm");
    let ctx = rt
        .block_on(gage_query::create_context(source.as_ref()))
        .map_err(|e| e.to_string())?;
    let mut warm_row_counts = RowCounts::default();
    for _ in 0..params.iterations {
        warm_row_counts = run_queries("warm", &rt, &ctx, params.limit, &mut timings)?;
        progress.inc(1);
    }
    emit_row_counts("warm", &warm_row_counts, &mut counts);
    drop(ctx);

    let sizes = vec![
        Size {
            name: "projects dir".into(),
            bytes: dir_size(&projects_dir),
        },
        Size {
            name: "summary cache".into(),
            bytes: dir_size(&cache_dir),
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

/// Row counts one iteration observed. Same numbers every iteration
/// on the same corpus; recorded once per scenario, not per iteration.
#[derive(Default)]
struct RowCounts {
    count_total: u64,
    list_light_rows: u64,
    list_full_rows: u64,
}

fn run_queries(
    scenario: &str,
    rt: &tokio::runtime::Runtime,
    ctx: &SessionContext,
    limit: usize,
    timings: &mut Timings,
) -> Result<RowCounts, String> {
    let count_sql = "SELECT COUNT(*) FROM session";
    let count = time_query(&format!("{scenario}.count"), rt, ctx, count_sql, timings)?;
    let count_total = first_int64(&count).unwrap_or(0).max(0) as u64;

    let list_light_sql =
        format!("SELECT id, project, mtime FROM session ORDER BY mtime DESC LIMIT {limit}");
    let light = time_query(
        &format!("{scenario}.list_light"),
        rt,
        ctx,
        &list_light_sql,
        timings,
    )?;
    let list_light_rows: u64 = light.iter().map(|b| b.num_rows() as u64).sum();

    let list_full_sql = format!(
        "SELECT id, project, title, model, size, message_count, mtime \
         FROM session ORDER BY mtime DESC LIMIT {limit}"
    );
    let full = time_query(
        &format!("{scenario}.list_full"),
        rt,
        ctx,
        &list_full_sql,
        timings,
    )?;
    let list_full_rows: u64 = full.iter().map(|b| b.num_rows() as u64).sum();

    Ok(RowCounts {
        count_total,
        list_light_rows,
        list_full_rows,
    })
}

fn emit_row_counts(scenario: &str, rows: &RowCounts, counts: &mut Vec<Count>) {
    counts.push(Count {
        name: format!("{scenario}.count.total"),
        value: rows.count_total,
    });
    counts.push(Count {
        name: format!("{scenario}.list_light.rows"),
        value: rows.list_light_rows,
    });
    counts.push(Count {
        name: format!("{scenario}.list_full.rows"),
        value: rows.list_full_rows,
    });
}

fn time_query(
    name: &str,
    rt: &tokio::runtime::Runtime,
    ctx: &SessionContext,
    sql: &str,
    timings: &mut Timings,
) -> Result<Vec<arrow::record_batch::RecordBatch>, String> {
    let ctx = ctx.clone();
    let sql = sql.to_string();
    let start = Instant::now();
    let batches = rt
        .block_on(async move { execute(&ctx, &sql).await })
        .map_err(|e| e.to_string())?;
    timings.record(name, start.elapsed().as_secs_f64() * 1000.0);
    Ok(batches)
}

async fn execute(
    ctx: &SessionContext,
    sql: &str,
) -> Result<Vec<arrow::record_batch::RecordBatch>, DataFusionError> {
    let df = ctx.sql(sql).await?;
    df.collect().await
}

fn first_int64(batches: &[arrow::record_batch::RecordBatch]) -> Option<i64> {
    let batch = batches.first()?;
    let array = batch.column(0).as_any().downcast_ref::<Int64Array>()?;
    if array.is_empty() {
        None
    } else {
        Some(array.value(0))
    }
}

/// Generate `params.sessions` files distributed round-robin across
/// `params.projects` slugs. Each file's contents come from
/// [`Generator::session_content`], sized to `params.session_kb * 1024`.
fn populate(projects_dir: &Path, params: &Params, progress: &ProgressBar) -> std::io::Result<()> {
    let mut generator = Generator::new(params.seed);
    let mut uuid_rng = StdRng::seed_from_u64(params.seed);
    let projects = params.projects.max(1);
    for i in 0..params.sessions {
        let slug = format!("-tmp-bench-project-{:03}", i % projects);
        let dir = projects_dir.join(&slug);
        if i < projects {
            std::fs::create_dir_all(&dir)?;
        }
        let uuid = fake_uuid(&mut uuid_rng);
        let path = dir.join(format!("{uuid}.jsonl"));
        let content = generator.session_content(params.session_kb * 1024);
        let mut file = std::fs::File::create(&path)?;
        file.write_all(content.as_bytes())?;
        progress.inc(1);
    }
    Ok(())
}

fn fake_uuid(rng: &mut StdRng) -> String {
    let a: u32 = rng.random();
    let b: u16 = rng.random();
    let c: u16 = rng.random();
    let d: u16 = rng.random();
    let e: u64 = rng.random_range(0..(1u64 << 48));
    format!("{a:08x}-{b:04x}-{c:04x}-{d:04x}-{e:012x}")
}

fn dir_size(dir: &Path) -> u64 {
    let mut total: u64 = 0;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total = total.saturating_add(dir_size(&path));
        } else if let Ok(meta) = entry.metadata() {
            total = total.saturating_add(meta.len());
        }
    }
    total
}
