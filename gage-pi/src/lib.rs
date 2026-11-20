//! The Pi driver: Pi coding agent sessions as Gage reads them.
//!
//! A Pi session is one file,
//! `<agent dir>/sessions/--<encoded cwd>--/<timestamp>_<id>.jsonl`,
//! one JSON object per line. The first line is a header naming the
//! format version and the working directory. Every later line is an
//! entry, and from format version 2 on an entry names its parent, so
//! one file holds every branch of the session.
//!
//! The driver reads and stores sessions, installs Gage into Pi, and
//! runs agents and skill sessions on it. It does not delete or move
//! sessions.

mod agent;
mod body;
mod codec;
mod content_schema;
mod corpus;
mod derive;
pub mod driver;
mod harness;
mod home;
mod model;
mod plugin;
mod session_file;
mod skill;
mod text_descriptor;
mod tool_use;

#[cfg(test)]
mod testing;
