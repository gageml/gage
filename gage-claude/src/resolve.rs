//! Builder for the interactive `/gage:resolve` Claude Code session,
//! shared by `gage resolve` and the scan view's resolve action.

use std::io;
use std::process::Command;

use crate::proc::find_claude;

/// Command that starts an interactive Claude Code session running the
/// gage resolve skill, scoped to `issue_ids` (all pending and open
/// issues when empty). The session gets a fresh session ID up front so
/// its display name can carry the short form. `model` maps to
/// `--model` and is required — a claude session never starts on the
/// user's default model; `extra_args` pass to claude verbatim, ahead
/// of the prompt.
pub fn resolve_command(
    issue_ids: &[String],
    model: &str,
    extra_args: &[String],
) -> io::Result<Command> {
    let claude_bin = find_claude()?;

    let mut prompt = String::from("/gage:resolve");
    for id in issue_ids {
        prompt.push(' ');
        prompt.push_str(id);
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(claude_bin);
    cmd.arg("--session-id").arg(&session_id);
    cmd.arg("-n")
        .arg(format!("Resolve Gage issues {}", &session_id[..8]));
    cmd.arg("--model").arg(model);
    cmd.args(extra_args);
    cmd.arg(prompt);
    Ok(cmd)
}
