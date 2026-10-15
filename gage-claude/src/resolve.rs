//! Builder for the interactive `/gage:resolve` Claude Code session,
//! shared by `gage resolve` and the scan view's resolve action.

use std::io;
use std::process::Command;

use crate::proc::find_claude;

/// Command that starts an interactive Claude Code session running the
/// gage resolve skill, scoped to `issue_ids` (all pending and open
/// issues when empty). `model` maps to `--model` and is required — a
/// claude session never starts on the user's default model;
/// `extra_args` pass to claude verbatim, ahead of the prompt.
pub(crate) fn resolve_command(
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

    let mut cmd = Command::new(claude_bin);
    cmd.arg("-n").arg("Resolve Gage issues");
    cmd.arg("--model").arg(model);
    cmd.args(extra_args);
    cmd.arg(prompt);
    Ok(cmd)
}
