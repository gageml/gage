//! The `store` bench: populate, measure, verify, and time reads.
//!
//! Phases run in order against one fresh store under a temporary home:
//!
//! 1. Populate: notes, sessions, large sessions, and datasets from the
//!    seeded generator, then edits, growth, and deletes on a fraction.
//!    Every operation is timed individually.
//! 2. Stats: repository and index sizes before and after `gc`.
//! 3. Verify: every object reads back as written, counts by type match,
//!    and every commit's parents are accounted for. A failure aborts
//!    the run.
//! 4. Reads: the common read operations, each repeated `iterations`
//!    times.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use gage_store::{DatasetStore, NoteInput, NoteStore, Order, SessionSpec, SessionStore, Store};
use indicatif::ProgressBar;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Serialize;

use crate::measure::{Count, Metric, Size, Timings};
use crate::report::Results;
use crate::synth::{Generator, SyntheticSession};

/// Scale and shape of one run.
#[derive(Debug, Clone, Serialize)]
pub struct Params {
    pub notes: usize,
    pub note_bytes: usize,
    pub sessions: usize,
    pub session_kb: usize,
    pub large_sessions: usize,
    pub large_kb: usize,
    pub datasets: usize,
    pub dataset_size: usize,
    pub edit_pct: u8,
    pub delete_pct: u8,
    pub iterations: usize,
    /// Reads by id drawn uniformly from the whole population.
    pub random_reads: usize,
    pub seed: u64,
}

const DRIVER: &str = "bench";
const DRIVER_VERSION: &str = "0";
const INDEX_FILE: &str = "cache/object-index.sqlite";

/// Everything the populate phase created, for verification and reads.
struct Population {
    note_ids: Vec<String>,
    /// Note id to the value it should read back as.
    note_values: BTreeMap<String, String>,
    deleted_notes: Vec<String>,
    /// Native session id to its content, after growth.
    sessions: Vec<(String, String)>,
    large_sessions: Vec<(String, String)>,
    /// Gage object ids of every session, regular and large.
    session_ids: Vec<String>,
    dataset_ids: Vec<String>,
    /// Dataset holding the large sessions.
    large_dataset: Option<String>,
}

pub const BENCH_NAME: &str = "store";

pub fn run(
    params: &Params,
    stamp: &str,
    home: &Path,
    progress: &ProgressBar,
) -> Result<Results, String> {
    let store_path = home.join("store.git");
    gage_store::init(&store_path).map_err(|e| e.to_string())?;
    let mut timings = Timings::default();
    let mut sizes = Vec::new();
    let mut counts = Vec::new();

    let store = Store::open(&store_path).map_err(|e| e.to_string())?;
    let population = populate(&store, params, &mut timings, progress)?;
    drop(store);

    measure_sizes(&store_path, home, "before gc", &mut sizes, &mut counts)?;
    progress.set_message("gc");
    let store = Store::open(&store_path).map_err(|e| e.to_string())?;
    timings.time("gc", || {
        store.gc(Some("now"), true).map_err(|e| e.to_string())
    })?;
    drop(store);
    measure_sizes(&store_path, home, "after gc", &mut sizes, &mut counts)?;

    progress.set_message("verify");
    let store = Store::open(&store_path).map_err(|e| e.to_string())?;
    let fsck = verify(&store, params, &population)?;
    drop(store);

    progress.set_message("reads");
    let read_metrics = time_reads(&store_path, home, params, &population)?;

    let mut metrics = timings.summarize();
    metrics.extend(read_metrics);
    Ok(Results {
        bench: BENCH_NAME.to_string(),
        stamp: stamp.to_string(),
        code_version: crate::report::code_version(),
        params: serde_json::to_value(params).map_err(|e| e.to_string())?,
        metrics,
        sizes,
        counts,
        fsck,
    })
}

fn populate(
    store: &Store,
    params: &Params,
    timings: &mut Timings,
    progress: &ProgressBar,
) -> Result<Population, String> {
    let mut generator = Generator::new(params.seed);
    let notes = NoteStore::from(store);
    let sessions = SessionStore::from(store);
    let datasets = DatasetStore::from(store);
    let total = params.notes + params.sessions + params.large_sessions + params.datasets;
    progress.set_length(total as u64);
    progress.set_message("populate");

    let mut population = Population {
        note_ids: Vec::with_capacity(params.notes),
        note_values: BTreeMap::new(),
        deleted_notes: Vec::new(),
        sessions: Vec::with_capacity(params.sessions),
        large_sessions: Vec::with_capacity(params.large_sessions),
        session_ids: Vec::with_capacity(params.sessions + params.large_sessions),
        dataset_ids: Vec::with_capacity(params.datasets),
        large_dataset: None,
    };

    for _ in 0..params.notes {
        let name = generator.note_name();
        let value = generator.text(params.note_bytes);
        let id = timings
            .time("note create", || {
                notes.create(NoteInput {
                    name,
                    value: &value,
                    author: "user:bench",
                    targets: &[],
                })
            })
            .map_err(|e| e.to_string())?;
        population.note_values.insert(id.clone(), value);
        population.note_ids.push(id);
        progress.inc(1);
    }

    for i in 0..params.sessions {
        let native_id = format!("session-{i:06}");
        let content = generator.session_content(params.session_kb * 1024);
        let mut reader = SyntheticSession::new(&native_id, &content);
        let outcome = timings
            .time("session add", || {
                sessions.add(DRIVER, DRIVER_VERSION, &mut reader)
            })
            .map_err(|e| e.to_string())?;
        population.session_ids.push(outcome.id);
        population.sessions.push((native_id, content));
        progress.inc(1);
    }

    for i in 0..params.large_sessions {
        let native_id = format!("large-{i:06}");
        let content = generator.session_content(params.large_kb * 1024);
        let mut reader = SyntheticSession::new(&native_id, &content);
        let outcome = timings
            .time("session add (large)", || {
                sessions.add(DRIVER, DRIVER_VERSION, &mut reader)
            })
            .map_err(|e| e.to_string())?;
        population.session_ids.push(outcome.id);
        population.large_sessions.push((native_id, content));
        progress.inc(1);
    }

    for d in 0..params.datasets {
        let id = timings
            .time("dataset create", || datasets.create())
            .map_err(|e| e.to_string())?;
        let start = (d * params.dataset_size) % params.sessions.max(1);
        let members: Vec<&(String, String)> = population
            .sessions
            .iter()
            .cycle()
            .skip(start)
            .take(params.dataset_size.min(params.sessions))
            .collect();
        let mut readers: Vec<SyntheticSession> = members
            .iter()
            .map(|(native_id, content)| SyntheticSession::new(native_id, content))
            .collect();
        let specs: Vec<SessionSpec<'_>> = readers
            .iter_mut()
            .map(|r| SessionSpec {
                driver_name: DRIVER,
                driver_version: DRIVER_VERSION,
                reader: r,
            })
            .collect();
        timings
            .time("dataset sessions add", || datasets.sessions_add(&id, specs))
            .map_err(|e| e.to_string())?;
        population.dataset_ids.push(id);
        progress.inc(1);
    }

    if !population.large_sessions.is_empty() {
        let id = datasets.create().map_err(|e| e.to_string())?;
        let mut readers: Vec<SyntheticSession> = population
            .large_sessions
            .iter()
            .map(|(native_id, content)| SyntheticSession::new(native_id, content))
            .collect();
        let specs: Vec<SessionSpec<'_>> = readers
            .iter_mut()
            .map(|r| SessionSpec {
                driver_name: DRIVER,
                driver_version: DRIVER_VERSION,
                reader: r,
            })
            .collect();
        timings
            .time("dataset sessions add (large)", || {
                datasets.sessions_add(&id, specs)
            })
            .map_err(|e| e.to_string())?;
        population.large_dataset = Some(id);
    }

    let edit_count = params.notes * usize::from(params.edit_pct) / 100;
    progress.set_message("edit");
    for id in population.note_ids.iter().take(edit_count) {
        let value = generator.text(params.note_bytes);
        timings
            .time("note edit", || notes.edit(id, &value))
            .map_err(|e| e.to_string())?;
        population.note_values.insert(id.clone(), value);
    }

    let grow_count = params.sessions * usize::from(params.edit_pct) / 100;
    progress.set_message("grow");
    for (native_id, content) in population.sessions.iter_mut().take(grow_count) {
        generator.grow(content, 20);
        let mut reader = SyntheticSession::new(native_id, content);
        timings
            .time("session grow", || {
                sessions.add(DRIVER, DRIVER_VERSION, &mut reader)
            })
            .map_err(|e| e.to_string())?;
    }

    let delete_count = params.notes * usize::from(params.delete_pct) / 100;
    progress.set_message("delete");
    for id in population.note_ids.iter().rev().take(delete_count) {
        timings
            .time("note delete", || notes.delete(id))
            .map_err(|e| e.to_string())?;
        population.deleted_notes.push(id.clone());
    }

    Ok(population)
}

fn measure_sizes(
    store_path: &Path,
    home: &Path,
    label: &str,
    sizes: &mut Vec<Size>,
    counts: &mut Vec<Count>,
) -> Result<(), String> {
    let store = Store::open(store_path).map_err(|e| e.to_string())?;
    let status = store.status().map_err(|e| e.to_string())?;
    let objects = status.loose_objects + status.packed_objects;
    let index_bytes = std::fs::metadata(home.join(INDEX_FILE))
        .map(|m| m.len())
        .unwrap_or(0);
    sizes.push(Size {
        name: format!("repository ({label})"),
        bytes: status.size,
    });
    sizes.push(Size {
        name: format!("bytes per git object ({label})"),
        bytes: status.size.checked_div(objects).unwrap_or(0),
    });
    sizes.push(Size {
        name: format!("index ({label})"),
        bytes: index_bytes,
    });
    counts.push(Count {
        name: format!("git objects ({label})"),
        value: objects,
    });
    counts.push(Count {
        name: format!("refs ({label})"),
        value: status.refs,
    });
    Ok(())
}

/// Returns the lines `git fsck` printed.
fn verify(store: &Store, params: &Params, population: &Population) -> Result<Vec<String>, String> {
    let notes = NoteStore::from(store);
    let sessions = SessionStore::from(store);
    let datasets = DatasetStore::from(store);

    for id in &population.note_ids {
        let (_, sha) = store
            .resolve_id(id)
            .map_err(|e| format!("verify: resolve {id}: {e}"))?;
        let object = store
            .read_object(&sha)
            .map_err(|e| format!("verify: read {id}: {e}"))?;
        if object.header.object_type != "gage::note" {
            return Err(format!("verify: {id} is a {}", object.header.object_type));
        }
        let deleted = population.deleted_notes.contains(id);
        if object.header.is_tombstone() != deleted {
            return Err(format!("verify: {id} tombstone state is wrong"));
        }
    }

    let live_notes = params.notes - population.deleted_notes.len();
    let counted = notes.iter().map_err(|e| e.to_string())?.count();
    if counted != live_notes {
        return Err(format!(
            "verify: expected {live_notes} live notes, found {counted}"
        ));
    }
    let counted = sessions.iter().map_err(|e| e.to_string())?.count();
    let expected = params.sessions + params.large_sessions;
    if counted != expected {
        return Err(format!(
            "verify: expected {expected} sessions, found {counted}"
        ));
    }
    let counted = datasets.iter().map_err(|e| e.to_string())?.count();
    let expected = params.datasets + usize::from(population.large_dataset.is_some());
    if counted != expected {
        return Err(format!(
            "verify: expected {expected} datasets, found {counted}"
        ));
    }

    let sample = population
        .note_ids
        .iter()
        .filter(|id| !population.deleted_notes.contains(id))
        .step_by((live_notes / 50).max(1));
    for id in sample {
        let full = notes
            .get(id)
            .map_err(|e| format!("verify: get {id}: {e}"))?;
        if Some(&full.value) != population.note_values.get(id) {
            return Err(format!("verify: {id} value differs from what was written"));
        }
    }

    for r in store.list_object_refs().map_err(|e| e.to_string())? {
        let parents = store
            .classify_parents(&r.tip_sha)
            .map_err(|e| format!("verify: parents of {}: {e}", r.id))?;
        if !parents.unattributed.is_empty() || !parents.missing.is_empty() {
            return Err(format!("verify: {} has unaccounted parents", r.id));
        }
    }

    let fsck = store.fsck().map_err(|e| format!("verify: git fsck: {e}"))?;

    for id in &population.dataset_ids {
        let listed = datasets
            .sessions_list(id)
            .map_err(|e| format!("verify: dataset {id}: {e}"))?;
        let expected = params.dataset_size.min(params.sessions);
        if listed.len() != expected {
            return Err(format!(
                "verify: dataset {id} has {} sessions, expected {expected}",
                listed.len()
            ));
        }
    }
    Ok(fsck)
}
fn time_reads(
    store_path: &Path,
    home: &Path,
    params: &Params,
    population: &Population,
) -> Result<Vec<Metric>, String> {
    let mut timings = Timings::default();
    let index_path: PathBuf = home.join(INDEX_FILE);

    for _ in 0..params.iterations {
        let store = timings.time("open (warm index)", || Store::open(store_path));
        drop(store.map_err(|e| e.to_string())?);
    }
    // A rebuild walks every object and runs for seconds at default
    // scale, so it is measured once rather than per iteration.
    std::fs::remove_file(&index_path).map_err(|e| e.to_string())?;
    let store = timings.time("open (rebuild index)", || Store::open(store_path));
    drop(store.map_err(|e| e.to_string())?);

    let store = Store::open(store_path).map_err(|e| e.to_string())?;
    let notes = NoteStore::from(&store);
    let datasets = DatasetStore::from(&store);
    let live: Vec<&String> = population
        .note_ids
        .iter()
        .filter(|id| !population.deleted_notes.contains(id))
        .collect();
    let pick = |i: usize| {
        live.get(i % live.len())
            .expect("index is reduced modulo the length")
    };

    for i in 0..params.iterations {
        let id = pick(i * 7);
        timings
            .time("resolve id (full)", || store.resolve_id(id))
            .map_err(|e| e.to_string())?;
        timings
            .time("resolve id (8-char prefix)", || store.resolve_id(&id[..8]))
            .map_err(|e| e.to_string())?;
        timings
            .time("note get", || notes.get(id))
            .map_err(|e| e.to_string())?;
    }

    // Uniform draws over every live object, seeded so two runs read
    // the same ids in the same order.
    let mut rng = StdRng::seed_from_u64(params.seed);
    let mut all_ids: Vec<&String> = live.clone();
    all_ids.extend(population.session_ids.iter());
    all_ids.extend(population.dataset_ids.iter());
    all_ids.extend(population.large_dataset.iter());
    for _ in 0..params.random_reads {
        let id = all_ids
            .get(rng.random_range(0..all_ids.len()))
            .expect("range is bounded by the length");
        timings
            .time("random read by id", || {
                let (_, sha) = store.resolve_id(id)?;
                store.read_object(&sha)
            })
            .map_err(|e| e.to_string())?;
    }
    for _ in 0..params.random_reads {
        let id = live
            .get(rng.random_range(0..live.len()))
            .expect("range is bounded by the length");
        timings
            .time("random note get", || notes.get(id))
            .map_err(|e| e.to_string())?;
    }

    for _ in 0..params.iterations {
        let count = timings
            .time("query name, limit 20", || {
                notes
                    .query()
                    .name("finding")
                    .limit(20)
                    .iter()
                    .and_then(|it| it.collect::<Result<Vec<_>, _>>())
                    .map(|v| v.len())
            })
            .map_err(|e| e.to_string())?;
        if count == 0 && live.len() >= 100 {
            return Err("query name returned nothing".to_string());
        }
    }

    for _ in 0..params.iterations {
        timings
            .time("iter all notes", || {
                notes
                    .iter()
                    .and_then(|it| it.collect::<Result<Vec<_>, _>>())
                    .map(|v| v.len())
            })
            .map_err(|e| e.to_string())?;
    }

    for _ in 0..params.iterations {
        timings
            .time("query modified desc, limit 20", || {
                notes
                    .query()
                    .order(Order::ModifiedDesc)
                    .limit(20)
                    .iter()
                    .and_then(|it| it.collect::<Result<Vec<_>, _>>())
                    .map(|v| v.len())
            })
            .map_err(|e| e.to_string())?;
    }

    if let Some(dataset) = population.dataset_ids.first() {
        for _ in 0..params.iterations {
            timings
                .time("dataset sessions list", || datasets.sessions_list(dataset))
                .map_err(|e| e.to_string())?;
        }
        for _ in 0..params.iterations {
            timings
                .time("session content read", || {
                    read_content(&datasets, dataset, 1)
                })
                .map_err(|e| e.to_string())?;
        }
    }
    if let Some(dataset) = &population.large_dataset {
        for _ in 0..params.iterations {
            timings
                .time("session content read (large)", || {
                    read_content(&datasets, dataset, 1)
                })
                .map_err(|e| e.to_string())?;
        }
    }

    Ok(timings.summarize())
}

/// Stream one member session's content to the end, returning the byte
/// count.
fn read_content(datasets: &DatasetStore, dataset: &str, num: u32) -> Result<u64, String> {
    let access = datasets
        .session_content(dataset, num)
        .map_err(|e| e.to_string())?;
    let mut total = 0u64;
    for path in access.paths().map_err(|e| e.to_string())? {
        let mut reader = access.open(&path).map_err(|e| e.to_string())?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            total += n as u64;
        }
    }
    Ok(total)
}
