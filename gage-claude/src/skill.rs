//! Builder for the Claude Code session that runs a Gage skill, shared
//! by `gage resolve`, `gage publish`, and the scan view's resolve
//! action.

use std::io;
use std::path::Path;
use std::process::Command;

use gage_session::{SkillMode, SkillSession};

use crate::driver::{MCP_SERVER_NAME, PLUGIN_NAME};
use crate::proc::find_claude;

/// The `claude` command that runs `session`. An interactive session
/// is named and takes no permission flags, since the user answers
/// prompts. A batch session is `claude -p`, where anything that would
/// prompt is denied, so it carries a standing grant of the tools a
/// Gage skill uses: `gage publish` and `gage session` commands and
/// the Gage MCP tools. `model` maps to `--model`; `None` passes none,
/// so claude applies its own model resolution. The extra arguments
/// pass to claude verbatim, ahead of the prompt.
pub(crate) fn skill_command(session: &SkillSession<'_>) -> io::Result<Command> {
    let claude_bin = find_claude()?;
    Ok(build_skill_command(&claude_bin, session))
}

fn build_skill_command(claude_bin: &Path, session: &SkillSession<'_>) -> Command {
    let mut cmd = Command::new(claude_bin);
    match session.mode {
        SkillMode::Interactive => {
            cmd.arg("-n").arg(session.name);
        }
        SkillMode::Batch => {
            cmd.arg("-p");
            cmd.arg("--allowedTools").args(batch_allowed_tools());
        }
    }
    if let Some(model) = session.model {
        cmd.arg("--model").arg(model);
    }
    cmd.args(session.extra_args);
    cmd.arg(skill_prompt(session.skill, session.args));
    cmd
}

/// The tools a batch skill session may call without a prompt.
fn batch_allowed_tools() -> [String; 3] {
    [
        "Bash(gage publish *)".to_string(),
        "Bash(gage session *)".to_string(),
        format!("mcp__plugin_{PLUGIN_NAME}_{MCP_SERVER_NAME}"),
    ]
}

/// The prompt that invokes `skill` with `args`: the slash command and
/// each argument as one shell word, so the skill can hand them to a
/// `gage` command as given.
fn skill_prompt(skill: &str, args: &[String]) -> String {
    let mut prompt = format!("/{skill}");
    for arg in args {
        prompt.push(' ');
        prompt.push_str(&shell_word(arg));
    }
    prompt
}

/// `arg` as one POSIX shell word: as is when it is made of safe
/// characters, otherwise single-quoted with each `'` closed, escaped,
/// and reopened.
fn shell_word(arg: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "-_./:@=+,%".contains(c);
    if !arg.is_empty() && arg.chars().all(safe) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    fn session<'a>(
        args: &'a [String],
        model: Option<&'a str>,
        extra: &'a [String],
        mode: SkillMode,
    ) -> SkillSession<'a> {
        SkillSession {
            name: "Resolve Gage issues",
            skill: "gage:resolve",
            args,
            model,
            extra_args: extra,
            mode,
        }
    }

    fn args(session: &SkillSession<'_>) -> Vec<String> {
        build_skill_command(Path::new("claude"), session)
            .get_args()
            .map(|a| a.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn interactive_names_the_session_and_passes_no_model_when_none_given() {
        let ids = ["abc".to_string()];
        let extra = ["--verbose".to_string()];
        let args = args(&session(&ids, None, &extra, SkillMode::Interactive));
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
        let ids = ["abc".to_string()];
        let extra = ["--verbose".to_string()];
        assert_eq!(
            args(&session(
                &ids,
                Some("claude-opus-4-8"),
                &extra,
                SkillMode::Interactive
            )),
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

    /// A batch session prints and exits, grants the Gage tools and no
    /// other, and carries no display name.
    #[test]
    fn batch_prints_with_the_gage_tools_allowed() {
        let skill_args = ["--stored".to_string(), "abc".to_string()];
        let session = SkillSession {
            name: "Publish sessions",
            skill: "gage:publish",
            args: &skill_args,
            model: None,
            extra_args: &[],
            mode: SkillMode::Batch,
        };
        assert_eq!(
            args(&session),
            [
                "-p",
                "--allowedTools",
                "Bash(gage publish *)",
                "Bash(gage session *)",
                "mcp__plugin_gage_gage",
                "/gage:publish --stored abc"
            ]
        );
    }

    /// An argument with spaces or quotes reaches the skill as one
    /// shell word.
    #[test]
    fn skill_arguments_are_shell_words() {
        let words = [
            "--notes".to_string(),
            "For the blog's readers".to_string(),
            String::new(),
            "a/b:c@d".to_string(),
        ];
        assert_eq!(
            skill_prompt("gage:publish", &words),
            r"/gage:publish --notes 'For the blog'\''s readers' '' a/b:c@d"
        );
    }

    #[test]
    fn program_is_claude_bin() {
        let cmd = build_skill_command(
            Path::new("/opt/claude"),
            &session(&[], None, &[], SkillMode::Interactive),
        );
        assert_eq!(cmd.get_program(), OsStr::new("/opt/claude"));
    }
}
