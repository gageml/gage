//! Canonical input/output shapes for the bench drivers. These types
//! carry serde derives only; format adapters that need their own
//! (borsh, bitcode-native, rkyv, speedy, …) define local mirror types
//! and conversions inside their own file. This module never depends
//! on any specific format crate.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintRec {
    pub mtime_ms: i64,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub title: Option<String>,
    pub model: Option<String>,
    pub message_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub is_empty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryRecord {
    pub session_id: String,
    pub fingerprint: FingerprintRec,
    pub summary: Summary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub session_id: String,
    pub line: i64,
    pub uuid: Option<String>,
    pub kind: Option<String>,
    pub subtype: Option<String>,
    pub timestamp_ms: Option<i64>,
    pub raw: String,
    pub text: Option<String>,
    pub attachments: Option<String>,
    pub ide_tags: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntriesRecord {
    pub session_id: String,
    pub fingerprint: FingerprintRec,
    pub entries: Vec<Entry>,
}
