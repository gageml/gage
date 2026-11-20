//! Builder for the Pi session that runs a Gage skill, shared by
//! `gage resolve`, `gage publish`, and the scan view's resolve action.

use std::path::Path;
use std::process::{Command, Stdio};

use gage_session::{ModelSizes, SkillMode, SkillSession};

use crate::plugin::target;

/// The `pi` command that runs `session`. The prompt is the skill's
/// command and its arguments. An interactive session is `pi` with
/// that prompt, named for the session. A batch session is `pi -p`,
/// which runs the prompt and exits. Pi asks for no permission before
/// a tool call, so a batch session needs no grant and has no sandbox:
/// its `gage` commands run as the user. A model that is a size
/// resolves under `sizes`; no model passes none, so Pi uses its own.
/// The extra arguments pass to `pi` verbatim, ahead of the prompt.
pub(crate) fn skill_command(pi: &Path, session: &SkillSession<'_>, sizes: &ModelSizes) -> Command {
    let mut cmd = Command::new(pi);
    match session.mode {
        SkillMode::Interactive => {
            cmd.arg("--name").arg(session.name);
        }
        SkillMode::Batch => {
            cmd.arg("--print");
            // Pi puts what it reads from a piped stdin in front of
            // the prompt
            cmd.stdin(Stdio::null());
        }
    }
    if let Some(model) = session.model {
        cmd.arg("--model").arg(sizes.get(model).unwrap_or(model));
    }
    cmd.args(session.extra_args);
    cmd.arg(target().skill_prompt(session.skill, session.args));
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::default_sizes;

    fn args(session: &SkillSession<'_>) -> Vec<String> {
        skill_command(Path::new("pi"), session, &default_sizes())
            .get_args()
            .map(|a| a.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn interactive_is_pi_named_for_the_session_with_the_skill_command_as_prompt() {
        let ids = ["abc".to_string()];
        let extra = ["--thinking".to_string(), "high".to_string()];
        let session = SkillSession {
            name: "Resolve Gage issues",
            skill: "resolve",
            args: &ids,
            model: None,
            extra_args: &extra,
            mode: SkillMode::Interactive,
        };
        assert_eq!(
            args(&session),
            [
                "--name",
                "Resolve Gage issues",
                "--thinking",
                "high",
                "/skill:gage-resolve abc"
            ]
        );
    }

    #[test]
    fn batch_prints_and_a_size_resolves_to_its_model() {
        let session = |model| SkillSession {
            name: "Publish",
            skill: "publish",
            args: &[],
            model,
            extra_args: &[],
            mode: SkillMode::Batch,
        };
        assert_eq!(
            args(&session(Some("small"))),
            [
                "--print",
                "--model",
                "anthropic/claude-haiku-4-5",
                "/skill:gage-publish"
            ]
        );
        assert_eq!(
            args(&session(Some("openai/gpt-6.1-terra")))[2],
            "openai/gpt-6.1-terra"
        );
        assert_eq!(args(&session(None)), ["--print", "/skill:gage-publish"]);
    }
}
