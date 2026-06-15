//! Cross-library serialization benchmark for gage's prospective
//! on-disk caches (per-session aggregates and per-session entries).
//!
//! Each compiled format is run against the same corpus; the runner
//! reports time (ser, deser), bytes on disk, and peak RSS during deser.

pub mod benches;
pub mod corpus;
pub mod formats;
pub mod measure;
pub mod model;
pub mod report;
