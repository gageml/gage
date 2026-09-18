//! Timing collection: one sample per operation, summarized per name.

use std::collections::BTreeMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Millisecond samples grouped by operation name, in insertion order.
#[derive(Default)]
pub struct Timings {
    samples: BTreeMap<String, Vec<f64>>,
    order: Vec<String>,
}

impl Timings {
    /// Time `f` and record the sample under `name`.
    pub fn time<R>(&mut self, name: &str, f: impl FnOnce() -> R) -> R {
        let start = Instant::now();
        let result = f();
        self.record(name, start.elapsed().as_secs_f64() * 1000.0);
        result
    }

    pub fn record(&mut self, name: &str, ms: f64) {
        if !self.samples.contains_key(name) {
            self.order.push(name.to_string());
        }
        self.samples.entry(name.to_string()).or_default().push(ms);
    }

    /// One summary per operation, in first-recorded order.
    pub fn summarize(&self) -> Vec<Metric> {
        self.order
            .iter()
            .map(|name| {
                let samples = self
                    .samples
                    .get(name)
                    .expect("order holds only recorded names");
                Metric::from_samples(name, samples)
            })
            .collect()
    }
}

/// Summary statistics for one operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub count: usize,
    pub total_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    pub per_sec: f64,
}

impl Metric {
    fn from_samples(name: &str, samples: &[f64]) -> Metric {
        let mut sorted = samples.to_vec();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let total_ms: f64 = sorted.iter().sum();
        let count = sorted.len();
        Metric {
            name: name.to_string(),
            count,
            total_ms,
            p50_ms: percentile(&sorted, 0.50),
            p95_ms: percentile(&sorted, 0.95),
            max_ms: sorted.last().copied().unwrap_or(0.0),
            per_sec: if total_ms > 0.0 {
                count as f64 / (total_ms / 1000.0)
            } else {
                0.0
            },
        }
    }
}

/// Nearest-rank percentile over ascending `sorted`.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted.get(rank - 1).copied().unwrap_or(0.0)
}

/// A byte count for one thing measured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Size {
    pub name: String,
    pub bytes: u64,
}

/// A plain count for one thing measured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Count {
    pub name: String,
    pub value: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank() {
        let s = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(percentile(&s, 0.50), 5.0);
        assert_eq!(percentile(&s, 0.95), 10.0);
        assert_eq!(percentile(&[7.0], 0.95), 7.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
    }

    #[test]
    fn summarize_keeps_first_recorded_order() {
        let mut t = Timings::default();
        t.record("b", 2.0);
        t.record("a", 1.0);
        t.record("b", 4.0);
        let m = t.summarize();
        let names: Vec<&str> = m.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["b", "a"]);
        let b = m.first().unwrap();
        assert_eq!(b.count, 2);
        assert_eq!(b.total_ms, 6.0);
    }
}
