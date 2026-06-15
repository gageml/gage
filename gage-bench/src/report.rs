//! Stdout table and JSON output for benchmark results.
//!
//! Numeric columns are rendered as `mean (rank)`. Rank is computed
//! per-column, lowest value = rank 1.
//!
//! Rows are sorted by bytes-on-disk, bucketed by single-linkage on a
//! relative-gap threshold so near-equal sizes (≤5% apart) cluster.
//! Within a bucket, `deser_ms` mean breaks the tie.

use std::cmp::Ordering;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use tabled::settings::Style;
use tabled::{Table, Tabled};

/// Two byte counts are considered equal-for-ranking when their
/// relative gap is ≤ this value. 5% maps the intuition that any
/// on-disk difference under 5% is a wash.
const SIZE_BUCKET_GAP_RATIO: f64 = 0.05;

#[derive(Debug, Clone, Serialize)]
pub struct RowData {
    pub bench: String,
    pub format: String,
    pub records: usize,
    pub ser_ms_samples: Vec<f64>,
    pub deser_ms_samples: Vec<f64>,
    pub bytes_on_disk: u64,
}

#[derive(Tabled)]
struct Row {
    bench: String,
    format: String,
    records: String,
    ser_ms: String,
    deser_ms: String,
    bytes: String,
}

pub fn print_table(rows: &[RowData]) {
    if rows.is_empty() {
        return;
    }

    let ser_means: Vec<f64> = rows.iter().map(|r| mean(&r.ser_ms_samples)).collect();
    let deser_means: Vec<f64> = rows.iter().map(|r| mean(&r.deser_ms_samples)).collect();
    let bytes_vals: Vec<f64> = rows.iter().map(|r| r.bytes_on_disk as f64).collect();

    let ser_ranks = ranks_asc(&ser_means);
    let deser_ranks = ranks_asc(&deser_means);
    let bytes_ranks = ranks_asc(&bytes_vals);

    let bytes_u64: Vec<u64> = rows.iter().map(|r| r.bytes_on_disk).collect();
    let bytes_buckets = relative_gap_buckets(&bytes_u64, SIZE_BUCKET_GAP_RATIO);

    let mut scored: Vec<(usize, f64, Row)> = rows
        .iter()
        .zip(ser_ranks)
        .zip(deser_ranks)
        .zip(bytes_ranks)
        .zip(bytes_buckets)
        .map(|((((r, sr), dr), br), bucket)| {
            let row = Row {
                bench: r.bench.clone(),
                format: r.format.clone(),
                records: r.records.to_string(),
                ser_ms: format_timing(&r.ser_ms_samples, sr),
                deser_ms: format_timing(&r.deser_ms_samples, dr),
                bytes: format_bytes_cell(r.bytes_on_disk, br),
            };
            (bucket, mean(&r.deser_ms_samples), row)
        })
        .collect();
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal))
    });
    let display: Vec<Row> = scored.into_iter().map(|(_, _, r)| r).collect();

    let mut table = Table::new(display);
    table.with(Style::sharp());
    println!("{table}");
}

pub fn write_json(path: &Path, rows: &[RowData]) -> std::io::Result<()> {
    let f = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(f, rows).map_err(std::io::Error::other)
}

pub fn duration_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn format_timing(samples: &[f64], rank: usize) -> String {
    let m = mean(samples);
    format!("{m:.2} ({rank})")
}

fn format_bytes_cell(bytes: u64, rank: usize) -> String {
    format!("{} ({rank})", format_bytes(bytes))
}

fn mean(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().sum::<f64>() / samples.len() as f64
}

fn ranks_asc(values: &[f64]) -> Vec<usize> {
    let mut indexed: Vec<(usize, f64)> = values.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
    let mut ranks = vec![0usize; values.len()];
    for (rank0, (idx, _)) in indexed.into_iter().enumerate() {
        if let Some(slot) = ranks.get_mut(idx) {
            *slot = rank0 + 1;
        }
    }
    ranks
}

fn format_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else if n < 1024u64.pow(3) {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Single-linkage 1D bucketing by relative gap: sort the values, then
/// start a new bucket whenever the gap to the previous value exceeds
/// `threshold` of that previous value. Returns one bucket index per
/// input position (in original order). Lower bucket index = smaller
/// values.
fn relative_gap_buckets(values: &[u64], threshold: f64) -> Vec<usize> {
    let n = values.len();
    let mut sorted: Vec<(usize, u64)> = values.iter().copied().enumerate().collect();
    sorted.sort_by_key(|(_, v)| *v);
    let mut buckets = vec![0usize; n];
    let mut current = 0usize;
    let mut prev: Option<u64> = None;
    for (orig_idx, v) in sorted {
        if let Some(p) = prev {
            let gap = (v as f64 - p as f64) / p.max(1) as f64;
            if gap > threshold {
                current += 1;
            }
        }
        if let Some(slot) = buckets.get_mut(orig_idx) {
            *slot = current;
        }
        prev = Some(v);
    }
    buckets
}
