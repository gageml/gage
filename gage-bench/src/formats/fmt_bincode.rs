use bincode::config;

use super::Format;
use crate::model::{EntriesRecord, SummaryRecord};

pub static FORMAT: Format = Format {
    name: "bincode-2",
    compress: true,
    encode_summary: enc_sum,
    decode_summary: dec_sum,
    encode_entries: enc_ent,
    decode_entries: dec_ent,
};

fn enc_sum(r: &SummaryRecord) -> Vec<u8> {
    bincode::serde::encode_to_vec(r, config::standard()).unwrap()
}

fn dec_sum(buf: &[u8]) -> SummaryRecord {
    let (v, _) = bincode::serde::decode_from_slice(buf, config::standard()).unwrap();
    v
}

fn enc_ent(r: &EntriesRecord) -> Vec<u8> {
    bincode::serde::encode_to_vec(r, config::standard()).unwrap()
}

fn dec_ent(buf: &[u8]) -> EntriesRecord {
    let (v, _) = bincode::serde::decode_from_slice(buf, config::standard()).unwrap();
    v
}
