//! Load real session data from `~/.claude/projects/` and convert it
//! into the bench-owned record shapes.

use std::path::PathBuf;

use arrow::array::{Array, Int64Array, StringArray, TimestampMillisecondArray};
use gage_claude::session::{SessionInfo, SessionListBuilder};
use gage_index::{
    COL_ATTACHMENTS, COL_IDE_TAGS, COL_LINE, COL_RAW, COL_SUBTYPE, COL_TEXT, COL_TIMESTAMP,
    COL_TYPE, COL_UUID, derive_session,
};
use indicatif::ProgressBar;
use rand::seq::SliceRandom;

use crate::model::{EntriesRecord, Entry, FingerprintRec, Summary, SummaryRecord};

pub struct Corpus {
    pub summaries: Vec<SummaryRecord>,
    pub entries: Vec<EntriesRecord>,
}

impl Corpus {
    pub fn session_count(&self) -> usize {
        self.summaries.len()
    }

    pub fn total_entries(&self) -> usize {
        self.entries.iter().map(|r| r.entries.len()).sum()
    }
}

pub struct LoadOptions {
    pub root: Option<PathBuf>,
    pub limit: Option<usize>,
}

/// Walk `~/.claude/projects/` and pick the session set to load: shuffle,
/// apply the optional limit. Cheap — does not parse any session file.
pub fn pick_sessions(opts: &LoadOptions) -> Result<Vec<SessionInfo>, String> {
    let mut builder = SessionListBuilder::new();
    if let Some(root) = opts.root.as_ref() {
        builder = builder.root(root.clone());
    }
    let mut sessions: Vec<SessionInfo> = builder.build().into_iter().collect();
    if sessions.is_empty() {
        return Err("no sessions found under ~/.claude/projects".to_string());
    }

    let mut rng = rand::rng();
    sessions.shuffle(&mut rng);
    if let Some(n) = opts.limit
        && sessions.len() > n
    {
        sessions.truncate(n);
    }
    Ok(sessions)
}

pub fn load(sessions: Vec<SessionInfo>, progress: &ProgressBar) -> Result<Corpus, String> {
    progress.set_length(sessions.len() as u64);
    progress.set_position(0);

    let mut summaries = Vec::with_capacity(sessions.len());
    let mut entries = Vec::with_capacity(sessions.len());
    for s in sessions {
        let derived = match derive_session(&s.id, &s.src) {
            Ok(d) => d,
            Err(e) => {
                progress.println(format!("Warn: derive {} failed: {e}", s.id));
                progress.inc(1);
                continue;
            }
        };

        let fingerprint = FingerprintRec {
            mtime_ms: derived.fingerprint.mtime_ms,
            size: derived.fingerprint.size,
        };

        summaries.push(SummaryRecord {
            session_id: derived.session_id.clone(),
            fingerprint: fingerprint.clone(),
            summary: Summary {
                title: derived.summary.title.clone(),
                model: derived.summary.model.clone(),
                message_count: derived.summary.message_count,
                input_tokens: derived.summary.input_tokens,
                output_tokens: derived.summary.output_tokens,
                cache_read_input_tokens: derived.summary.cache_read_input_tokens,
                cache_creation_input_tokens: derived.summary.cache_creation_input_tokens,
                is_empty: derived.summary.is_empty,
            },
        });

        entries.push(EntriesRecord {
            session_id: derived.session_id.clone(),
            fingerprint,
            entries: entries_from_batch(&derived.session_id, &derived.batch),
        });
        progress.inc(1);
    }

    if summaries.is_empty() {
        return Err("all sessions failed to derive".to_string());
    }

    Ok(Corpus { summaries, entries })
}

fn entries_from_batch(session_id: &str, batch: &arrow::record_batch::RecordBatch) -> Vec<Entry> {
    let n = batch.num_rows();
    let lines = col_i64(batch, COL_LINE);
    let uuids = col_str(batch, COL_UUID);
    let kinds = col_str(batch, COL_TYPE);
    let subtypes = col_str(batch, COL_SUBTYPE);
    let timestamps = col_ts(batch, COL_TIMESTAMP);
    let raws = col_str(batch, COL_RAW);
    let texts = col_str(batch, COL_TEXT);
    let attachments = col_str(batch, COL_ATTACHMENTS);
    let ide_tags = col_str(batch, COL_IDE_TAGS);

    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(Entry {
            session_id: session_id.to_string(),
            line: lines.value(i),
            uuid: uuids.and_then(|a| nullable_str(a, i)),
            kind: kinds.and_then(|a| nullable_str(a, i)),
            subtype: subtypes.and_then(|a| nullable_str(a, i)),
            timestamp_ms: timestamps.and_then(|a| nullable_ts(a, i)),
            raw: raws.map(|a| a.value(i).to_string()).unwrap_or_default(),
            text: texts.and_then(|a| nullable_str(a, i)),
            attachments: attachments.and_then(|a| nullable_str(a, i)),
            ide_tags: ide_tags.and_then(|a| nullable_str(a, i)),
        });
    }
    out
}

fn col_i64(batch: &arrow::record_batch::RecordBatch, idx: usize) -> &Int64Array {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("derived schema column should be Int64")
}

fn col_str(batch: &arrow::record_batch::RecordBatch, idx: usize) -> Option<&StringArray> {
    batch.column(idx).as_any().downcast_ref::<StringArray>()
}

fn col_ts(
    batch: &arrow::record_batch::RecordBatch,
    idx: usize,
) -> Option<&TimestampMillisecondArray> {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<TimestampMillisecondArray>()
}

fn nullable_str(arr: &StringArray, i: usize) -> Option<String> {
    if arr.is_null(i) {
        None
    } else {
        Some(arr.value(i).to_string())
    }
}

fn nullable_ts(arr: &TimestampMillisecondArray, i: usize) -> Option<i64> {
    if arr.is_null(i) {
        None
    } else {
        Some(arr.value(i))
    }
}
