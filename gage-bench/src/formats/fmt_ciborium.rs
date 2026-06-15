use super::Format;
use crate::model::{EntriesRecord, SummaryRecord};

pub static FORMAT: Format = Format {
    name: "ciborium",
    compress: true,
    encode_summary: enc_sum,
    decode_summary: dec_sum,
    encode_entries: enc_ent,
    decode_entries: dec_ent,
};

fn enc_sum(r: &SummaryRecord) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(r, &mut out).unwrap();
    out
}

fn dec_sum(buf: &[u8]) -> SummaryRecord {
    ciborium::from_reader(buf).unwrap()
}

fn enc_ent(r: &EntriesRecord) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(r, &mut out).unwrap();
    out
}

fn dec_ent(buf: &[u8]) -> EntriesRecord {
    ciborium::from_reader(buf).unwrap()
}
