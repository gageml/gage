//! Benchmarks for the Gage store.
//!
//! The `store` bench populates a fresh store with generated objects,
//! reports size statistics, verifies that everything reads back, and
//! times the common read operations. Results are written as JSON so a
//! later run can be compared against a baseline.

pub mod measure;
pub mod report;
pub mod store_bench;
pub mod synth;
