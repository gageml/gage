//! The Codex driver: OpenAI Codex CLI sessions as Gage reads them.
//!
//! A Codex session is one rollout file,
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`,
//! one JSON object per line. The driver reads the paginated history
//! mode, in which the harness records each command, MCP call, and file
//! change it ran as a structured item beside the model's own records.

mod body;
mod codec;
mod corpus;
mod derive;
pub mod driver;
mod home;
mod patch;
mod rollout;
mod text_descriptor;
mod tool_use;

#[cfg(test)]
mod testing;
