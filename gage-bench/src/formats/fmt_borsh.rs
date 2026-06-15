use borsh::{BorshDeserialize, BorshSerialize};

use super::Format;
use crate::model::{EntriesRecord, Entry, FingerprintRec, Summary, SummaryRecord};

#[derive(BorshSerialize, BorshDeserialize)]
struct Fp {
    mtime_ms: i64,
    size: u64,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct Sm {
    title: Option<String>,
    model: Option<String>,
    message_count: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_input_tokens: i64,
    cache_creation_input_tokens: i64,
    is_empty: bool,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct Sr {
    session_id: String,
    fingerprint: Fp,
    summary: Sm,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct En {
    session_id: String,
    line: i64,
    uuid: Option<String>,
    kind: Option<String>,
    subtype: Option<String>,
    timestamp_ms: Option<i64>,
    raw: String,
    text: Option<String>,
    attachments: Option<String>,
    ide_tags: Option<String>,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct Er {
    session_id: String,
    fingerprint: Fp,
    entries: Vec<En>,
}

impl From<&FingerprintRec> for Fp {
    fn from(v: &FingerprintRec) -> Self {
        Self {
            mtime_ms: v.mtime_ms,
            size: v.size,
        }
    }
}

impl From<Fp> for FingerprintRec {
    fn from(v: Fp) -> Self {
        Self {
            mtime_ms: v.mtime_ms,
            size: v.size,
        }
    }
}

impl From<&Summary> for Sm {
    fn from(v: &Summary) -> Self {
        Self {
            title: v.title.clone(),
            model: v.model.clone(),
            message_count: v.message_count,
            input_tokens: v.input_tokens,
            output_tokens: v.output_tokens,
            cache_read_input_tokens: v.cache_read_input_tokens,
            cache_creation_input_tokens: v.cache_creation_input_tokens,
            is_empty: v.is_empty,
        }
    }
}

impl From<Sm> for Summary {
    fn from(v: Sm) -> Self {
        Self {
            title: v.title,
            model: v.model,
            message_count: v.message_count,
            input_tokens: v.input_tokens,
            output_tokens: v.output_tokens,
            cache_read_input_tokens: v.cache_read_input_tokens,
            cache_creation_input_tokens: v.cache_creation_input_tokens,
            is_empty: v.is_empty,
        }
    }
}

impl From<&SummaryRecord> for Sr {
    fn from(v: &SummaryRecord) -> Self {
        Self {
            session_id: v.session_id.clone(),
            fingerprint: (&v.fingerprint).into(),
            summary: (&v.summary).into(),
        }
    }
}

impl From<Sr> for SummaryRecord {
    fn from(v: Sr) -> Self {
        Self {
            session_id: v.session_id,
            fingerprint: v.fingerprint.into(),
            summary: v.summary.into(),
        }
    }
}

impl From<&Entry> for En {
    fn from(v: &Entry) -> Self {
        Self {
            session_id: v.session_id.clone(),
            line: v.line,
            uuid: v.uuid.clone(),
            kind: v.kind.clone(),
            subtype: v.subtype.clone(),
            timestamp_ms: v.timestamp_ms,
            raw: v.raw.clone(),
            text: v.text.clone(),
            attachments: v.attachments.clone(),
            ide_tags: v.ide_tags.clone(),
        }
    }
}

impl From<En> for Entry {
    fn from(v: En) -> Self {
        Self {
            session_id: v.session_id,
            line: v.line,
            uuid: v.uuid,
            kind: v.kind,
            subtype: v.subtype,
            timestamp_ms: v.timestamp_ms,
            raw: v.raw,
            text: v.text,
            attachments: v.attachments,
            ide_tags: v.ide_tags,
        }
    }
}

impl From<&EntriesRecord> for Er {
    fn from(v: &EntriesRecord) -> Self {
        Self {
            session_id: v.session_id.clone(),
            fingerprint: (&v.fingerprint).into(),
            entries: v.entries.iter().map(Into::into).collect(),
        }
    }
}

impl From<Er> for EntriesRecord {
    fn from(v: Er) -> Self {
        Self {
            session_id: v.session_id,
            fingerprint: v.fingerprint.into(),
            entries: v.entries.into_iter().map(Into::into).collect(),
        }
    }
}

pub static FORMAT: Format = Format {
    name: "borsh",
    compress: true,
    encode_summary: enc_sum,
    decode_summary: dec_sum,
    encode_entries: enc_ent,
    decode_entries: dec_ent,
};

fn enc_sum(r: &SummaryRecord) -> Vec<u8> {
    borsh::to_vec(&Sr::from(r)).unwrap()
}

fn dec_sum(buf: &[u8]) -> SummaryRecord {
    borsh::from_slice::<Sr>(buf).unwrap().into()
}

fn enc_ent(r: &EntriesRecord) -> Vec<u8> {
    borsh::to_vec(&Er::from(r)).unwrap()
}

fn dec_ent(buf: &[u8]) -> EntriesRecord {
    borsh::from_slice::<Er>(buf).unwrap().into()
}
