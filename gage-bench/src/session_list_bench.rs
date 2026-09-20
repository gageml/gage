//! The `claude-session-list` bench: baseline for `gage session list`
//! performance against the Claude driver.
//!
//! One run against a temp directory holding synthetic Claude sessions:
//!
//! 1. **Populate.** Generate `sessions` synthetic JSONL files spread
//!    across `projects` project slugs. Not timed.
//! 2. **Cold scenario.** `iterations` runs of the list operation with
//!    the summary cache empty; each phase timed separately.
//! 3. **Reconcile.** Fill the summary cache once. Timed on its own line.
//! 4. **Warm scenario.** `iterations` runs with the summary cache
//!    populated; each phase timed separately.
//!
//! The list operation decomposes into four phases each iteration:
//!
//! - `enumerate` — driver enumerates every session under the source
//!   root, one `metadata()` per file, no content read.
//! - `cheap_attrs` — for each yielded session, read the cheap
//!   attributes (mtime, size, project_name).
//! - `sort_limit` — sort by mtime desc and truncate to `limit`.
//! - `summary_load` — for the surviving `limit` sessions, resolve
//!   `title`, `model`, `message_count`. Cold path derives from JSONL;
//!   warm path hits the on-disk summary cache.
//!
//! Metrics are named `<scenario>.<phase>`. `total` is the sum of the
//! four per iteration.

use std::io::Write;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gage_claude::driver::{ClaudeDriver, ClaudeNativeSession};
use gage_index::{IndexStore, LockMode, SessionSummary, derive_session};
use gage_session::{Driver, NativeSession, SourceUrl};
use indicatif::ProgressBar;
use rand::{Rng, SeedableRng, rngs::StdRng};
use serde::Serialize;

use crate::measure::{Count, Size, Timings};
use crate::report::{self, Results};
use crate::synth::Generator;

pub const BENCH_NAME: &str = "claude-session-list";

/// Scale and shape of one bench run.
#[derive(Debug, Clone, Serialize)]
pub struct Params {
    /// Total session files to generate under `projects/`.
    pub sessions: usize,
    /// Number of project slugs the sessions are distributed across.
    pub projects: usize,
    /// KiB of JSONL content per session file.
    pub session_kb: usize,
    /// List `--limit` value for each iteration.
    pub limit: usize,
    /// Iterations of the list operation per scenario.
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
    let cache_dir = run_dir.join("cache");
    std::fs::create_dir_all(&projects_dir).map_err(|e| e.to_string())?;

    progress.set_length((params.sessions + params.iterations * 2 + 1) as u64);
    progress.set_message("populate");
    populate(&projects_dir, params, progress).map_err(|e| e.to_string())?;

    let source = SourceUrl::new("claude", run_dir.to_string_lossy().into_owned());
    let driver = ClaudeDriver::new();
    // IndexStore's `root` is the projects directory (that's what
    // SessionListBuilder walks); the driver's source body is the
    // Claude home root (the driver appends `/projects`).
    let index = IndexStore::new(&projects_dir, &cache_dir);

    let mut timings = Timings::default();
    let mut counts = Vec::new();

    progress.set_message("cold");
    for _ in 0..params.iterations {
        run_list(
            "cold",
            &driver,
            &source,
            &index,
            params.limit,
            &mut timings,
            &mut counts,
        )?;
        progress.inc(1);
    }

    progress.set_message("reconcile");
    let reconcile_start = Instant::now();
    let outcome = index.reconcile(LockMode::Wait).map_err(|e| e.to_string())?;
    timings.record(
        "reconcile",
        reconcile_start.elapsed().as_secs_f64() * 1000.0,
    );
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
    for _ in 0..params.iterations {
        run_list(
            "warm",
            &driver,
            &source,
            &index,
            params.limit,
            &mut timings,
            &mut counts,
        )?;
        progress.inc(1);
    }

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

/// One iteration of the list operation, timed per phase.
fn run_list(
    scenario: &str,
    driver: &dyn Driver,
    source: &SourceUrl,
    index: &IndexStore,
    limit: usize,
    timings: &mut Timings,
    counts: &mut Vec<Count>,
) -> Result<(), String> {
    // Phase 1: enumerate.
    let start = Instant::now();
    let sessions: Vec<Box<dyn NativeSession>> = driver
        .sessions(source)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let enumerate_ms = start.elapsed().as_secs_f64() * 1000.0;
    timings.record(&format!("{scenario}.enumerate"), enumerate_ms);

    // Phase 2: cheap-attrs read.
    let start = Instant::now();
    let attrs: Vec<CheapAttrs> = sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let a = s.attrs();
            CheapAttrs {
                session_idx: idx,
                mtime: a.mtime().unwrap_or(UNIX_EPOCH),
            }
        })
        .collect();
    let cheap_attrs_ms = start.elapsed().as_secs_f64() * 1000.0;
    timings.record(&format!("{scenario}.cheap_attrs"), cheap_attrs_ms);

    // Phase 3: sort + limit.
    let start = Instant::now();
    let mut sorted = attrs;
    sorted.sort_by_key(|a| std::cmp::Reverse(a.mtime));
    sorted.truncate(limit);
    let sort_limit_ms = start.elapsed().as_secs_f64() * 1000.0;
    timings.record(&format!("{scenario}.sort_limit"), sort_limit_ms);

    // Phase 4: summary load. Cold path never touches the cache; warm
    // path hits the on-disk cache and falls back to a derive on miss.
    let start = Instant::now();
    let mut cache_hits: u64 = 0;
    let mut cache_misses: u64 = 0;
    for a in &sorted {
        let session = sessions
            .get(a.session_idx)
            .expect("session_idx came from an enumeration of `sessions`");
        let (_, hit) = load_summary(scenario, session.as_ref(), a.mtime, index);
        if hit {
            cache_hits += 1;
        } else {
            cache_misses += 1;
        }
    }
    let summary_load_ms = start.elapsed().as_secs_f64() * 1000.0;
    timings.record(&format!("{scenario}.summary_load"), summary_load_ms);

    counts.push(Count {
        name: format!("{scenario}.cache_hits"),
        value: cache_hits,
    });
    counts.push(Count {
        name: format!("{scenario}.cache_misses"),
        value: cache_misses,
    });

    timings.record(
        &format!("{scenario}.total"),
        enumerate_ms + cheap_attrs_ms + sort_limit_ms + summary_load_ms,
    );
    Ok(())
}

struct CheapAttrs {
    session_idx: usize,
    mtime: SystemTime,
}

/// Load one session's summary. Returns `(summary, cache_hit)`.
/// Cold scenario always derives from JSONL. Warm scenario consults
/// `IndexStore::session_summary` first and falls back to a derive.
fn load_summary(
    scenario: &str,
    session: &dyn NativeSession,
    mtime: SystemTime,
    index: &IndexStore,
) -> (SessionSummary, bool) {
    let claude = session
        .as_any()
        .downcast_ref::<ClaudeNativeSession>()
        .expect("ClaudeDriver yields ClaudeNativeSession");
    let native_id = session.native_id();
    let session_path = claude.session_path();

    if scenario == "warm"
        && let Some(cached) = index.session_summary(native_id, mtime)
    {
        return (cached, true);
    }
    let summary = derive_session(native_id, session_path)
        .map(|d| d.summary)
        .unwrap_or_default();
    (summary, false)
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
