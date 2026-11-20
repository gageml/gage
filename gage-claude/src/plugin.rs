//! Claude Code as the Gage plugin is rendered for it.

use gage_plugin::{PluginTarget, SkillNames, SubagentModels};

/// What Claude Code puts in front of a Gage MCP tool's name: a
/// plugin's MCP tools are `mcp__plugin_<plugin>_<server>__<tool>`.
pub(crate) const TOOL_PREFIX: &str = "mcp__plugin_gage_gage__";

/// The plugin for Claude Code: its sub-agents run on models pinned
/// here, since a definition names one model and not a size.
pub(crate) fn target() -> PluginTarget<'static> {
    PluginTarget {
        agent: crate::driver::HARNESS_NAME,
        tool_prefix: TOOL_PREFIX,
        source_scheme: crate::driver::NAME,
        skill_names: SkillNames::Namespaced,
        skill_invoke: "/",
        subagents: Some(SubagentModels {
            reconcile: "claude-sonnet-4-6",
            redact: "claude-haiku-5-5",
        }),
        plugin_manifest: ".claude-plugin/plugin.json",
        marketplace_manifest: Some(".claude-plugin/marketplace.json"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use gage_plugin::write_plugin;

    use super::*;

    #[test]
    fn the_plugin_is_written_in_claude_codes_layout() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path(), &target()).unwrap();

        let manifest =
            fs::read_to_string(dir.path().join(".claude-plugin").join("plugin.json")).unwrap();
        assert!(manifest.contains("\"name\": \"gage\""));
        assert!(manifest.contains(&format!("\"version\": \"{}\"", gage_plugin::VERSION)));
        let marketplace =
            fs::read_to_string(dir.path().join(".claude-plugin").join("marketplace.json")).unwrap();
        assert!(marketplace.contains("\"source\": \"./\""));
        assert!(dir.path().join("agents").join("redact.md").exists());
        let resolve =
            fs::read_to_string(dir.path().join("skills").join("resolve").join("SKILL.md")).unwrap();
        assert!(resolve.contains("`mcp__plugin_gage_gage__issue_assign`"));
    }
}
