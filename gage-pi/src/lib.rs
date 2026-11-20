//! The Pi driver: Pi coding agent sessions as Gage reads them.
//!
//! A Pi session is one file,
//! `<agent dir>/sessions/--<encoded cwd>--/<timestamp>_<id>.jsonl`,
//! one JSON object per line. The first line is a header naming the
//! format version and the working directory. Every later line is an
//! entry, and from format version 2 on an entry names its parent, so
//! one file holds every branch of the session.
//!
//! The driver reads and stores sessions. It does not delete or move
//! them, install Gage into Pi, or run agents on it.

mod body;
mod codec;
mod content_schema;
mod corpus;
mod derive;
pub mod driver;
mod home;
mod session_file;
mod text_descriptor;
mod tool_use;

#[cfg(test)]
mod testing;
