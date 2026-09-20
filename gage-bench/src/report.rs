//! Stdout tables, JSON output, and baseline comparison.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use tabled::settings::Style;
use tabled::{Table, Tabled};

use crate::measure::{Count, Metric, Size};

/// Everything one run produced, as written to `results.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Results {
    /// Bench name; results are kept under `bench/<bench>/` in Gage home.
    pub bench: String,
    /// UTC timestamp of the run, also the saved file's stem.
    pub stamp: String,
    /// Gage version and commit the bench was built from, e.g.
    /// `0.2.0-dev (e0e835a)`.
    pub code_version: String,
    pub params: serde_json::Value,
    pub metrics: Vec<Metric>,
    pub sizes: Vec<Size>,
    pub counts: Vec<Count>,
    /// Every line `git fsck` printed during verify. A passing fsck
    /// still reports notices, and they are kept with the run.
    #[serde(default)]
    pub fsck: Vec<String>,
}

impl Results {
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let file = fs::File::create(path)?;
        serde_json::to_writer_pretty(file, self).map_err(io::Error::other)
    }

    pub fn read(path: &Path) -> io::Result<Results> {
        let file = fs::File::open(path)?;
        serde_json::from_reader(file).map_err(io::Error::other)
    }

    /// Save under `gage-bench/results/<bench>/<stamp>.json` in the
    /// source tree and return the path.
    pub fn save(&self) -> io::Result<PathBuf> {
        let dir = results_dir(&self.bench);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", self.stamp));
        self.write(&path)?;
        Ok(path)
    }
}

/// Where saved results for `bench` live: `results/<bench>/` under
/// this crate's directory in the source tree, so a baseline can be
/// committed with the change it measured.
pub fn results_dir(bench: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("results")
        .join(bench)
}

/// The newest saved results file for `bench`, by file name, which is
/// the run stamp.
pub fn latest_results(bench: &str) -> io::Result<Option<PathBuf>> {
    let dir = results_dir(bench);
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths.pop())
}

/// The gage version this binary was built from, with the commit of the
/// working tree when git can report it: `0.2.0-dev (e0e835a)`, plus
/// `-dirty` when the tree has uncommitted changes. Without git the
/// version alone is reported.
pub fn code_version() -> String {
    let version = env!("CARGO_PKG_VERSION");
    // Run in this crate's directory, not the caller's, so the answer
    // is the workspace's commit wherever the bench is invoked from
    let head = Command::new("git")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let Some(head) = head else {
        return version.to_string();
    };
    let dirty = Command::new("git")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.is_empty());
    if dirty {
        format!("{version} ({head}-dirty)")
    } else {
        format!("{version} ({head})")
    }
}

#[derive(Tabled)]
struct MetricRow {
    operation: String,
    count: usize,
    total_ms: String,
    p50_ms: String,
    p95_ms: String,
    max_ms: String,
    per_sec: String,
}

pub fn print_metrics(title: &str, metrics: &[Metric]) {
    if metrics.is_empty() {
        return;
    }
    println!();
    println!("== {title} ==");
    let rows: Vec<MetricRow> = metrics
        .iter()
        .map(|m| MetricRow {
            operation: m.name.clone(),
            count: m.count,
            total_ms: format!("{:.1}", m.total_ms),
            p50_ms: format!("{:.2}", m.p50_ms),
            p95_ms: format!("{:.2}", m.p95_ms),
            max_ms: format!("{:.2}", m.max_ms),
            per_sec: format!("{:.1}", m.per_sec),
        })
        .collect();
    let mut table = Table::new(rows);
    table.with(Style::sharp());
    println!("{table}");
}

#[derive(Tabled)]
struct SizeRow {
    measure: String,
    size: String,
    bytes: u64,
}

pub fn print_sizes(title: &str, sizes: &[Size]) {
    if sizes.is_empty() {
        return;
    }
    println!();
    println!("== {title} ==");
    let rows: Vec<SizeRow> = sizes
        .iter()
        .map(|s| SizeRow {
            measure: s.name.clone(),
            size: format_bytes(s.bytes),
            bytes: s.bytes,
        })
        .collect();
    let mut table = Table::new(rows);
    table.with(Style::sharp());
    println!("{table}");
}

#[derive(Tabled)]
struct CountRow {
    measure: String,
    value: u64,
}

pub fn print_counts(title: &str, counts: &[Count]) {
    if counts.is_empty() {
        return;
    }
    println!();
    println!("== {title} ==");
    let rows: Vec<CountRow> = counts
        .iter()
        .map(|c| CountRow {
            measure: c.name.clone(),
            value: c.value,
        })
        .collect();
    let mut table = Table::new(rows);
    table.with(Style::sharp());
    println!("{table}");
}

pub fn print_fsck(lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    println!();
    println!("== fsck ==");
    for line in lines {
        println!("{line}");
    }
}

#[derive(Tabled)]
struct DeltaRow {
    name: String,
    baseline: String,
    current: String,
    delta: String,
}

/// Print `p50_ms` and byte deltas of `current` against `baseline`,
/// matched by name. Names present in only one run are listed with a
/// blank on the missing side. Differing params are reported first,
/// since deltas across different scales do not compare.
pub fn print_comparison(baseline: &Results, current: &Results) {
    println!();
    println!(
        "== Baseline: {} {} ==",
        baseline.stamp, baseline.code_version
    );
    if baseline.params != current.params {
        println!("Warning: params differ from the baseline; deltas are not comparable");
        println!("  baseline: {}", baseline.params);
        println!("  current:  {}", current.params);
    }
    println!();
    println!("== Compared with baseline (p50 ms) ==");
    let rows = delta_rows(
        baseline.metrics.iter().map(|m| (m.name.clone(), m.p50_ms)),
        current.metrics.iter().map(|m| (m.name.clone(), m.p50_ms)),
        |v| format!("{v:.2}"),
    );
    let mut table = Table::new(rows);
    table.with(Style::sharp());
    println!("{table}");

    println!();
    println!("== Compared with baseline (bytes) ==");
    let rows = delta_rows(
        baseline
            .sizes
            .iter()
            .map(|s| (s.name.clone(), s.bytes as f64)),
        current
            .sizes
            .iter()
            .map(|s| (s.name.clone(), s.bytes as f64)),
        |v| format_bytes(v as u64),
    );
    let mut table = Table::new(rows);
    table.with(Style::sharp());
    println!("{table}");
}

fn delta_rows(
    baseline: impl Iterator<Item = (String, f64)>,
    current: impl Iterator<Item = (String, f64)>,
    fmt: impl Fn(f64) -> String,
) -> Vec<DeltaRow> {
    let base: Vec<(String, f64)> = baseline.collect();
    let cur: Vec<(String, f64)> = current.collect();
    let mut rows = Vec::new();
    for (name, now) in &cur {
        let before = base.iter().find(|(n, _)| n == name).map(|(_, v)| *v);
        rows.push(DeltaRow {
            name: name.clone(),
            baseline: before.map(&fmt).unwrap_or_default(),
            current: fmt(*now),
            delta: match before {
                Some(b) if b > 0.0 => format!("{:+.1}%", (now - b) / b * 100.0),
                _ => String::new(),
            },
        });
    }
    for (name, before) in &base {
        if !cur.iter().any(|(n, _)| n == name) {
            rows.push(DeltaRow {
                name: name.clone(),
                baseline: fmt(*before),
                current: String::new(),
                delta: String::new(),
            });
        }
    }
    rows
}

pub fn format_bytes(n: u64) -> String {
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
