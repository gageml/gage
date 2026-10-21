//! Builder for the interactive `/gage:resolve` Claude Code session,
//! shared by `gage resolve` and the scan view's resolve action.

use std::io;
use std::path::Path;
use std::process::Command;

use crate::proc::find_claude;

/// Command that starts an interactive Claude Code session running the
/// gage resolve skill, scoped to `issue_ids` (all pending and open
/// issues when empty). `model` maps to `--model`; `None` passes no
/// `--model`, so claude applies its own model resolution. `extra_args`
/// pass to claude verbatim, ahead of the prompt.
pub(crate) fn resolve_command(
    issue_ids: &[String],
    model: Option<&str>,
    extra_args: &[String],
) -> io::Result<Command> {
    let claude_bin = find_claude()?;
    Ok(build_resolve_command(
        &claude_bin,
        issue_ids,
        model,
        extra_args,
    ))
}

fn build_resolve_command(
    claude_bin: &Path,
    issue_ids: &[String],
    model: Option<&str>,
    extra_args: &[String],
) -> Command {
    let mut prompt = String::from("/gage:resolve");
    for id in issue_ids {
        prompt.push(' ');
        prompt.push_str(id);
    }

    let mut cmd = Command::new(claude_bin);
    cmd.arg("-n").arg("Resolve Gage issues");
    if let Some(model) = model {
        cmd.arg("--model").arg(model);
    }
    cmd.args(extra_args);
    cmd.arg(prompt);
    cmd
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    fn args(model: Option<&str>) -> Vec<String> {
        let ids = ["abc".to_string()];
        let extra = ["--verbose".to_string()];
        build_resolve_command(Path::new("claude"), &ids, model, &extra)
            .get_args()
            .map(|a| a.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn no_model_passes_no_model_arg() {
        let args = args(None);
        assert!(!args.iter().any(|a| a == "--model"), "{args:?}");
        assert_eq!(
            args,
            [
                "-n",
                "Resolve Gage issues",
                "--verbose",
                "/gage:resolve abc"
            ]
        );
    }

    #[test]
    fn given_model_passes_model_arg_before_extra_args() {
        assert_eq!(
            args(Some("claude-opus-4-8")),
            [
                "-n",
                "Resolve Gage issues",
                "--model",
                "claude-opus-4-8",
                "--verbose",
                "/gage:resolve abc"
            ]
        );
    }

    #[test]
    fn program_is_claude_bin() {
        let cmd = build_resolve_command(Path::new("/opt/claude"), &[], None, &[]);
        assert_eq!(cmd.get_program(), OsStr::new("/opt/claude"));
    }
}
