//! Format trait and compiled-in registry. A format is just a pair of
//! "encode one record" / "decode one record" closures for each record
//! type we benchmark. Adding a format means adding one file in this
//! module and listing it in `compiled()`.

use crate::model::{EntriesRecord, SummaryRecord};

pub struct Format {
    pub name: &'static str,
    /// Whether the bench driver should zstd-compress this format's
    /// output before writing it. False only for explicit baselines.
    pub compress: bool,
    pub encode_summary: fn(&SummaryRecord) -> Vec<u8>,
    pub decode_summary: fn(&[u8]) -> SummaryRecord,
    pub encode_entries: fn(&EntriesRecord) -> Vec<u8>,
    pub decode_entries: fn(&[u8]) -> EntriesRecord,
}

pub fn compiled() -> Vec<&'static Format> {
    vec![
        #[cfg(feature = "fmt-bincode")]
        &fmt_bincode::FORMAT,
        #[cfg(feature = "fmt-bitcode")]
        &fmt_bitcode::FORMAT,
        #[cfg(feature = "fmt-borsh")]
        &fmt_borsh::FORMAT,
        #[cfg(feature = "fmt-ciborium")]
        &fmt_ciborium::FORMAT,
        #[cfg(feature = "fmt-flexbuffers")]
        &fmt_flexbuffers::FORMAT,
        #[cfg(feature = "fmt-json")]
        &fmt_json::FORMAT,
        #[cfg(feature = "fmt-json-uncompressed")]
        &fmt_json_uncompressed::FORMAT,
        #[cfg(feature = "fmt-postcard")]
        &fmt_postcard::FORMAT,
        #[cfg(feature = "fmt-pot")]
        &fmt_pot::FORMAT,
        #[cfg(feature = "fmt-rkyv")]
        &fmt_rkyv::FORMAT,
        #[cfg(feature = "fmt-rmp-serde")]
        &fmt_rmp_serde::FORMAT,
        #[cfg(feature = "fmt-speedy")]
        &fmt_speedy::FORMAT,
    ]
}

#[cfg(feature = "fmt-bincode")]
mod fmt_bincode;
#[cfg(feature = "fmt-bitcode")]
mod fmt_bitcode;
#[cfg(feature = "fmt-borsh")]
mod fmt_borsh;
#[cfg(feature = "fmt-ciborium")]
mod fmt_ciborium;
#[cfg(feature = "fmt-flexbuffers")]
mod fmt_flexbuffers;
#[cfg(feature = "fmt-json")]
mod fmt_json;
#[cfg(feature = "fmt-json-uncompressed")]
mod fmt_json_uncompressed;
#[cfg(feature = "fmt-postcard")]
mod fmt_postcard;
#[cfg(feature = "fmt-pot")]
mod fmt_pot;
#[cfg(feature = "fmt-rkyv")]
mod fmt_rkyv;
#[cfg(feature = "fmt-rmp-serde")]
mod fmt_rmp_serde;
#[cfg(feature = "fmt-speedy")]
mod fmt_speedy;
