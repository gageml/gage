//! Benchmark drivers. Each driver takes the corpus + the list of
//! compiled formats and returns one result row per format.

pub mod entry;
pub mod session;

use std::path::Path;

use indicatif::ProgressBar;

use crate::corpus::Corpus;
use crate::formats::Format;
use crate::report::RowData;

/// zstd level the bench compresses at. Level 3 is the default used by
/// most caches — balanced ratio and speed.
const ZSTD_LEVEL: i32 = 3;

pub fn write_payload(path: &Path, bytes: &[u8], compress: bool) {
    if compress {
        let compressed = zstd::encode_all(bytes, ZSTD_LEVEL).unwrap();
        std::fs::write(path, &compressed).unwrap();
    } else {
        std::fs::write(path, bytes).unwrap();
    }
}

pub fn read_payload(path: &Path, compress: bool) -> Vec<u8> {
    let raw = std::fs::read(path).unwrap();
    if compress {
        zstd::decode_all(raw.as_slice()).unwrap()
    } else {
        raw
    }
}

pub struct Bench {
    pub name: &'static str,
    pub description: &'static str,
    pub run: fn(&Corpus, &[&'static Format], &Path, usize, &ProgressBar) -> Vec<RowData>,
}

pub fn registry() -> &'static [Bench] {
    &[
        Bench {
            name: "session",
            description: "round-trip per-session summary",
            run: session::run,
        },
        Bench {
            name: "entry",
            description: "round-trip per-session entries",
            run: entry::run,
        },
    ]
}

pub fn find(name: &str) -> Option<&'static Bench> {
    registry().iter().find(|b| b.name == name)
}
