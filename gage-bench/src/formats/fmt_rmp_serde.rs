use super::Format;
use crate::model::{EntriesRecord, SummaryRecord};

pub static FORMAT: Format = Format {
    name: "rmp-serde",
    compress: true,
    encode_summary: enc_sum,
    decode_summary: dec_sum,
    encode_entries: enc_ent,
    decode_entries: dec_ent,
};

fn enc_sum(r: &SummaryRecord) -> Vec<u8> {
    rmp_serde::to_vec(r).unwrap()
}

fn dec_sum(buf: &[u8]) -> SummaryRecord {
    rmp_serde::from_slice(buf).unwrap()
}

fn enc_ent(r: &EntriesRecord) -> Vec<u8> {
    rmp_serde::to_vec(r).unwrap()
}

fn dec_ent(buf: &[u8]) -> EntriesRecord {
    rmp_serde::from_slice(buf).unwrap()
}
