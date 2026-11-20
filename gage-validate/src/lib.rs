//! The harness validation suite.
//!
//! A run downloads native session files from the upstream sources
//! `sources.toml` lists, at each source's latest revision, reads the
//! ones it lists from this repository, and checks the driver of each
//! file's harness against every one of them: the
//! driver finds the session, the store holds its bytes, the stored
//! session writes the same bytes back, each entry encodes to the line
//! it was decoded from, and the native and stored tables agree. The
//! run is a batch a person starts; no test of the workspace reaches
//! the network.

pub mod check;
mod error;
pub mod fetch;
pub mod harness;
pub mod manifest;
pub mod report;

pub use error::Error;
