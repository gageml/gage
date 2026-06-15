//! Per-session aggregates round-trip. Each iteration writes one file
//! per session and reads them all back; we collect a sample per
//! iteration for mean/stddev reporting.

use std::path::Path;

use indicatif::ProgressBar;

use super::{read_payload, write_payload};
use crate::corpus::Corpus;
use crate::formats::Format;
use crate::measure::{dir_bytes, time};
use crate::model::SummaryRecord;
use crate::report::{RowData, duration_ms};

pub fn run(
    corpus: &Corpus,
    formats: &[&'static Format],
    outdir: &Path,
    iterations: usize,
    progress: &ProgressBar,
) -> Vec<RowData> {
    let mut rows = Vec::with_capacity(formats.len());
    for fmt in formats {
        let fmt_dir = outdir.join("session").join(fmt.name);
        std::fs::create_dir_all(&fmt_dir).unwrap();

        progress.set_message(fmt.name.to_string());

        let mut ser_samples = Vec::with_capacity(iterations);
        let mut deser_samples = Vec::with_capacity(iterations);
        let mut last_records = 0usize;

        for _ in 0..iterations {
            let (_, ser_time) = time(|| {
                for rec in &corpus.summaries {
                    let bytes = (fmt.encode_summary)(rec);
                    let path = fmt_dir.join(format!("{}.bin", rec.session_id));
                    write_payload(&path, &bytes, fmt.compress);
                    progress.inc(1);
                }
            });
            ser_samples.push(duration_ms(ser_time));

            let (decoded, deser_time) = time(|| {
                let mut out: Vec<SummaryRecord> = Vec::with_capacity(corpus.summaries.len());
                for rec in &corpus.summaries {
                    let path = fmt_dir.join(format!("{}.bin", rec.session_id));
                    let buf = read_payload(&path, fmt.compress);
                    out.push((fmt.decode_summary)(&buf));
                    progress.inc(1);
                }
                out
            });
            deser_samples.push(duration_ms(deser_time));
            last_records = decoded.len();
            drop(decoded);
        }

        let bytes = dir_bytes(&fmt_dir);

        rows.push(RowData {
            bench: "session".to_string(),
            format: fmt.name.to_string(),
            records: last_records,
            ser_ms_samples: ser_samples,
            deser_ms_samples: deser_samples,
            bytes_on_disk: bytes,
        });
    }
    rows
}
