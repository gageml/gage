//! Builder for the Codex session that runs a Gage skill, shared by
//! `gage resolve`, `gage publish`, and the scan view's resolve action.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use gage_core::config::gage_home;
use gage_session::{ModelSizes, SkillMode, SkillSession};

use crate::plugin::target;

/// The `codex` command that runs `session`. The prompt is the skill's
/// mention and its arguments. An interactive session is `codex` with
/// that prompt; the user answers what it asks, and Codex has no option
/// that names a session, so the session's display name is not used. A
/// batch session is `codex exec`, which asks nothing: Gage's MCP tools
/// are approved by the plugin's own registration, and its `gage`
/// commands run in the workspace-write sandbox with `writable` added
/// to what they may write. A model that is a size resolves under
/// `sizes`; no model passes none, so Codex uses its own. The extra
/// arguments pass to `codex` verbatim, ahead of the prompt.
pub(crate) fn skill_command(
    codex: &Path,
    session: &SkillSession<'_>,
    sizes: &ModelSizes,
    writable: &[PathBuf],
) -> Command {
    let mut cmd = Command::new(codex);
    if session.mode == SkillMode::Batch {
        cmd.args([
            "exec",
            "--skip-git-repo-check",
            "--sandbox",
            "workspace-write",
        ]);
        for dir in writable {
            cmd.arg("--add-dir").arg(dir);
        }
        // `codex exec` reads a prompt from stdin when stdin is open
        cmd.stdin(Stdio::null());
    }
    if let Some(model) = session.model {
        cmd.arg("--model").arg(sizes.get(model).unwrap_or(model));
    }
    cmd.args(session.extra_args);
    cmd.arg(target().skill_prompt(session.skill, session.args));
    cmd
}

/// The directories a batch skill session's `gage` commands write
/// under: Gage's home, and the store when `GAGE_STORE` moves it out
/// of the home.
pub(crate) fn gage_writable_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![gage_home()];
    if let Some(store) = env::var_os("GAGE_STORE").filter(|s| !s.is_empty()) {
        dirs.push(PathBuf::from(store));
    }
    dirs
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;
    use crate::model::default_sizes;

    fn args(session: &SkillSession<'_>, writable: &[PathBuf]) -> Vec<String> {
        skill_command(Path::new("codex"), session, &default_sizes(), writable)
            .get_args()
            .map(|a| a.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn interactive_is_codex_with_the_skill_mention_as_prompt() {
        let ids = ["abc".to_string()];
        let extra = ["--search".to_string()];
        let session = SkillSession {
            name: "Resolve Gage issues",
            skill: "resolve",
            args: &ids,
            model: None,
            extra_args: &extra,
            mode: SkillMode::Interactive,
        };
        assert_eq!(args(&session, &[]), ["--search", "$gage:resolve abc"]);
    }

    #[test]
    fn a_size_resolves_to_its_model_and_a_name_passes_through() {
        let session = |model| SkillSession {
            name: "Resolve Gage issues",
            skill: "resolve",
            args: &[],
            model: Some(model),
            extra_args: &[],
            mode: SkillMode::Interactive,
        };
        assert_eq!(
            args(&session("small"), &[]),
            ["--model", "gpt-6-luna", "$gage:resolve"]
        );
        assert_eq!(
            args(&session("gpt-5.6-terra"), &[]),
            ["--model", "gpt-5.6-terra", "$gage:resolve"]
        );
    }

    /// A batch session runs to completion in the write sandbox with
    /// Gage's directories writable.
    #[test]
    fn batch_is_codex_exec_in_the_write_sandbox_with_gage_dirs_added() {
        let skill_args = ["--stored".to_string(), "it's".to_string()];
        let session = SkillSession {
            name: "Publish sessions",
            skill: "publish",
            args: &skill_args,
            model: None,
            extra_args: &[],
            mode: SkillMode::Batch,
        };
        let writable = [PathBuf::from("/home/u/.gage")];
        assert_eq!(
            args(&session, &writable),
            [
                "exec",
                "--skip-git-repo-check",
                "--sandbox",
                "workspace-write",
                "--add-dir",
                "/home/u/.gage",
                r"$gage:publish --stored 'it'\''s'"
            ]
        );
    }

    #[test]
    fn program_is_the_codex_binary() {
        let session = SkillSession {
            name: "Resolve Gage issues",
            skill: "resolve",
            args: &[],
            model: None,
            extra_args: &[],
            mode: SkillMode::Interactive,
        };
        let cmd = skill_command(Path::new("/opt/codex"), &session, &default_sizes(), &[]);
        assert_eq!(cmd.get_program(), OsStr::new("/opt/codex"));
    }
}
