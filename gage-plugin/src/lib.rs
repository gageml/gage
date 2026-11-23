//! The Gage plugin: the skills, sub-agent definitions, and manifests
//! Gage installs into a coding agent, kept once and rendered for each
//! agent.
//!
//! The files under `templates/` are Jinja templates. A driver
//! describes its agent as a [`PluginTarget`]: what the agent is
//! called, how it names Gage's MCP tools, which scheme its native
//! sessions have, how it names a skill and how a prompt invokes one,
//! and whether it runs the plugin's sub-agents. [`write_plugin`] renders the templates for
//! that target into a directory the agent installs from, and
//! [`write_mcp_json`] writes the MCP server registration beside them.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use minijinja::{AutoEscape, Environment, Error, ErrorKind, UndefinedBehavior, context};
use rust_embed::RustEmbed;
use serde_json::{Map, Value, json};

/// The plugin's name in every agent
pub const PLUGIN_NAME: &str = "gage";
/// The name of the marketplace that holds the plugin
pub const MARKETPLACE_NAME: &str = "gage";
/// The plugin as an agent's plugin commands name it:
/// `<plugin>@<marketplace>`
pub const PLUGIN_ID: &str = "gage@gage";
/// The name of the MCP server the plugin registers
pub const MCP_SERVER_NAME: &str = "gage";
/// The plugin version this build installs, the workspace version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const MCP_PATH: &str = ".mcp.json";
const PLUGIN_MANIFEST: &str = "plugin.json";
const MARKETPLACE_MANIFEST: &str = "marketplace.json";
const AGENTS_DIR: &str = "agents/";
const SKILLS_DIR: &str = "skills/";

/// The plugin's skills, by the names Gage gives them. An agent has
/// its own name for each, which [`PluginTarget::skill_name`] gives.
pub const SKILLS: &[&str] = &["blame", "publish", "resolve", "sidecar"];

#[derive(RustEmbed)]
#[folder = "templates/"]
struct Templates;

/// The agent a plugin is rendered for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTarget<'a> {
    /// The agent's display name, `Claude Code`
    pub agent: &'a str,
    /// What the agent puts in front of a Gage MCP tool's name when it
    /// shows the tool to the model, `mcp__gage__`
    pub tool_prefix: &'a str,
    /// The scheme of the agent's native sessions, `claude`
    pub source_scheme: &'a str,
    /// How the agent names the plugin's skills
    pub skill_names: SkillNames,
    /// What a prompt puts in front of a skill's name to invoke it,
    /// `/`
    pub skill_invoke: &'a str,
    /// The models of the plugin's sub-agents. `None` for an agent
    /// that does not run them: the sub-agent definitions are left out
    /// and the skills do those steps in the main session.
    pub subagents: Option<SubagentModels<'a>>,
    /// Where the agent reads the plugin manifest, relative to the
    /// plugin root
    pub plugin_manifest: &'a str,
    /// Where the agent reads the marketplace manifest, relative to
    /// the plugin root. `None` for an agent that installs the plugin
    /// from its own directory, with no marketplace.
    pub marketplace_manifest: Option<&'a str>,
}

/// How an agent names the skills of a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillNames {
    /// The agent names a skill by its plugin and its directory:
    /// `gage:blame`
    Namespaced,
    /// The agent keeps every skill it has under one set of names and
    /// reads a skill's from its `name` field, so the plugin's name is
    /// part of the name written there: `gage-blame`
    Global,
}

impl PluginTarget<'_> {
    /// The name the agent has for the plugin's skill `skill`, one of
    /// [`SKILLS`].
    pub fn skill_name(&self, skill: &str) -> String {
        match self.skill_names {
            SkillNames::Namespaced => format!("{PLUGIN_NAME}:{skill}"),
            SkillNames::Global => format!("{PLUGIN_NAME}-{skill}"),
        }
    }

    /// What invokes the plugin's skill `skill` at the start of a
    /// prompt: `/gage:blame`.
    pub fn skill_invocation(&self, skill: &str) -> String {
        format!("{}{}", self.skill_invoke, self.skill_name(skill))
    }

    /// The prompt that invokes the plugin's skill `skill` with
    /// `args`: the skill's invocation, then each argument as one
    /// shell word, so the skill can hand them to a `gage` command as
    /// given.
    pub fn skill_prompt(&self, skill: &str, args: &[String]) -> String {
        let mut prompt = self.skill_invocation(skill);
        for arg in args {
            prompt.push(' ');
            prompt.push_str(&shell_word(arg));
        }
        prompt
    }

    /// Where the file the template `name` renders goes, relative to
    /// the plugin root. A manifest goes where the agent reads it, and
    /// a skill of an agent with one set of skill names goes in a
    /// directory of the name the agent has for it. `None` for a file
    /// the agent does not take.
    fn path_of(&self, name: &str) -> Option<String> {
        if name.starts_with(AGENTS_DIR) && self.subagents.is_none() {
            return None;
        }
        if name == PLUGIN_MANIFEST {
            return Some(self.plugin_manifest.to_string());
        }
        if name == MARKETPLACE_MANIFEST {
            return self.marketplace_manifest.map(String::from);
        }
        let skill_file = name
            .strip_prefix(SKILLS_DIR)
            .and_then(|rest| rest.split_once('/'));
        match (self.skill_names, skill_file) {
            (SkillNames::Global, Some((skill, file))) => {
                Some(format!("{SKILLS_DIR}{}/{file}", self.skill_name(skill)))
            }
            _ => Some(name.to_string()),
        }
    }
}

/// The model each of the plugin's sub-agents runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentModels<'a> {
    /// The agent that judges a group of possibly duplicate issues
    pub reconcile: &'a str,
    /// The agent that finds text to redact in a publish candidate
    pub redact: &'a str,
}

/// Replace the contents of `root` with the plugin rendered for
/// `target`: the skills, the sub-agent definitions when the target
/// runs them, and the manifests at the paths the target names. The
/// directory is both the marketplace root and the plugin root.
pub fn write_plugin(root: &Path, target: &PluginTarget<'_>) -> io::Result<()> {
    if root.exists() {
        fs::remove_dir_all(root)?;
    }
    for (path, text) in render(target)? {
        let dest = root.join(path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(dest, text)?;
    }
    Ok(())
}

/// The plugin's files for `target`, each as its path under the plugin
/// root and its text, in path order.
pub fn render(target: &PluginTarget<'_>) -> io::Result<Vec<(String, String)>> {
    let mut env = Environment::new();
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_keep_trailing_newline(true);
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);
    let names = skill_table(|skill| target.skill_name(skill));
    env.add_function("skill_name", move |skill: &str| known_skill(&names, skill));
    let invocations = skill_table(|skill| target.skill_invocation(skill));
    env.add_function("skill_invocation", move |skill: &str| {
        known_skill(&invocations, skill)
    });
    let vars = context! {
        agent => target.agent,
        tool => target.tool_prefix,
        source => format!("{}:", target.source_scheme),
        named_skills => target.skill_names == SkillNames::Global,
        subagents => target.subagents.is_some(),
        models => target.subagents.map(|m| context! {
            reconcile => m.reconcile,
            redact => m.redact,
        }),
        version => VERSION,
    };
    let mut files = Vec::new();
    for name in Templates::iter() {
        let Some(path) = target.path_of(&name) else {
            continue;
        };
        let file = Templates::get(&name).expect("iter names embedded files");
        let source = std::str::from_utf8(&file.data).map_err(io::Error::other)?;
        let text = env
            .render_str(source, &vars)
            .map_err(|e| io::Error::other(format!("plugin template {name}: {e}")))?;
        files.push((path, text));
    }
    files.sort();
    Ok(files)
}

/// What `of` gives for each of the plugin's skills, by the skill's
/// name.
fn skill_table(of: impl Fn(&str) -> String) -> HashMap<&'static str, String> {
    SKILLS.iter().map(|skill| (*skill, of(skill))).collect()
}

/// The entry of `table` for `skill`. A template that names a skill
/// the plugin does not have is a mistake in the template.
fn known_skill(table: &HashMap<&'static str, String>, skill: &str) -> Result<String, Error> {
    table.get(skill).cloned().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidOperation,
            format!("{skill:?} is not a skill of the plugin"),
        )
    })
}

/// Write `.mcp.json` under `root`, registering the Gage MCP server
/// with the launch command `mcp_cmd`: the executable, then its
/// arguments. `settings` are further members of the server entry, for
/// what one agent reads there and another does not.
pub fn write_mcp_json(
    root: &Path,
    mcp_cmd: &[&str],
    settings: Map<String, Value>,
) -> io::Result<()> {
    let (program, args) = mcp_cmd
        .split_first()
        .expect("mcp_cmd should name an executable");
    let mut server = Map::new();
    server.insert("command".to_string(), json!(program));
    server.insert("args".to_string(), json!(args));
    server.extend(settings);
    let doc = json!({ "mcpServers": { MCP_SERVER_NAME: server } });
    let text = serde_json::to_string_pretty(&doc).map_err(io::Error::other)?;
    fs::create_dir_all(root)?;
    fs::write(root.join(MCP_PATH), text)
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
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    /// Claude Code as its driver describes it
    fn claude() -> PluginTarget<'static> {
        PluginTarget {
            agent: "Claude Code",
            tool_prefix: "mcp__plugin_gage_gage__",
            source_scheme: "claude",
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

    /// Codex as its driver describes it
    fn codex() -> PluginTarget<'static> {
        PluginTarget {
            agent: "Codex",
            tool_prefix: "mcp__gage__",
            source_scheme: "codex",
            skill_names: SkillNames::Namespaced,
            skill_invoke: "$",
            subagents: None,
            plugin_manifest: ".codex-plugin/plugin.json",
            marketplace_manifest: Some(".agents/plugins/marketplace.json"),
        }
    }

    /// Pi as its driver describes it
    fn pi() -> PluginTarget<'static> {
        PluginTarget {
            agent: "Pi",
            tool_prefix: "mcp__gage__",
            source_scheme: "pi",
            skill_names: SkillNames::Global,
            skill_invoke: "/skill:",
            subagents: None,
            plugin_manifest: "package.json",
            marketplace_manifest: None,
        }
    }

    fn rendered(target: &PluginTarget<'_>) -> BTreeMap<String, String> {
        render(target).unwrap().into_iter().collect()
    }

    /// The files under `testdata/<name>`, by path under it, with the
    /// version written as the build's
    fn snapshot(name: &str) -> BTreeMap<String, String> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name);
        let mut files = BTreeMap::new();
        let mut dirs = vec![root.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                let rel = path.strip_prefix(&root).unwrap().to_str().unwrap();
                let text = fs::read_to_string(&path).unwrap();
                files.insert(rel.to_string(), text.replace("%VERSION%", VERSION));
            }
        }
        files
    }

    /// Compare a rendering with its snapshot file by file. With
    /// `GAGE_UPDATE_PLUGIN_SNAPSHOTS` set, the snapshot is rewritten
    /// from the rendering instead.
    fn assert_matches_snapshot(name: &str, target: &PluginTarget<'_>) {
        let got = rendered(target);
        if std::env::var_os("GAGE_UPDATE_PLUGIN_SNAPSHOTS").is_some() {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("testdata")
                .join(name);
            if root.exists() {
                fs::remove_dir_all(&root).unwrap();
            }
            for (path, text) in &got {
                let dest = root.join(path);
                fs::create_dir_all(dest.parent().unwrap()).unwrap();
                fs::write(dest, text.replace(VERSION, "%VERSION%")).unwrap();
            }
            return;
        }
        let want = snapshot(name);
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>()
        );
        for (path, text) in &got {
            assert_eq!(text, &want[path], "{name}/{path}");
        }
    }

    #[test]
    fn the_claude_rendering_is_the_plugin_claude_code_has_always_had() {
        assert_matches_snapshot("claude", &claude());
    }

    #[test]
    fn the_codex_rendering_matches_its_snapshot() {
        assert_matches_snapshot("codex", &codex());
    }

    #[test]
    fn the_pi_rendering_matches_its_snapshot() {
        assert_matches_snapshot("pi", &pi());
    }

    /// An agent with one set of skill names gets each skill under the
    /// plugin's name, in the directory, in the `name` field the agent
    /// reads, and in the invocation the skill tells the user.
    #[test]
    fn a_skill_has_the_name_its_agent_gives_it() {
        assert_eq!(claude().skill_name("blame"), "gage:blame");
        assert_eq!(claude().skill_invocation("blame"), "/gage:blame");
        assert_eq!(codex().skill_invocation("resolve"), "$gage:resolve");
        assert_eq!(pi().skill_name("blame"), "gage-blame");
        assert_eq!(pi().skill_invocation("resolve"), "/skill:gage-resolve");

        let files = rendered(&pi());
        for skill in SKILLS {
            let text = &files[&format!("skills/gage-{skill}/SKILL.md")];
            assert!(
                text.starts_with(&format!("---\nname: gage-{skill}\n")),
                "{skill}: {text}"
            );
        }
        assert!(
            files["skills/gage-blame/SKILL.md"]
                .contains("`/skill:gage-blame FILE[:LINES] [question]`")
        );
        assert!(files.keys().all(|path| !path.contains("marketplace")));
        for target in [claude(), codex()] {
            let text = &rendered(&target)["skills/blame/SKILL.md"];
            assert!(text.starts_with("---\ndescription:"), "{text}");
        }
    }

    #[test]
    fn a_target_without_subagents_gets_no_agent_definitions_and_no_delegation() {
        let files = rendered(&codex());
        assert!(files.keys().all(|path| !path.starts_with("agents/")));
        for (path, text) in &files {
            assert!(!text.contains("gage:redact"), "{path}");
            assert!(!text.contains("subagent_type"), "{path}");
        }
        assert!(
            rendered(&claude())
                .keys()
                .any(|path| path == "agents/redact.md")
        );
    }

    #[test]
    fn a_rendering_names_only_its_own_agent() {
        for (path, text) in rendered(&codex()) {
            assert!(!text.contains("Claude"), "{path}");
            assert!(!text.contains("claude:"), "{path}");
            assert!(!text.contains("mcp__plugin_gage_gage__"), "{path}");
        }
        let blame = &rendered(&codex())["skills/blame/SKILL.md"];
        assert!(blame.contains("`mcp__gage__query`"));
        assert!(blame.contains("set to `codex:`"));
        assert!(blame.contains("`$gage:blame FILE[:LINES] [question]`"));
    }

    #[test]
    fn the_manifests_land_where_the_target_reads_them() {
        let files = rendered(&codex());
        let manifest: Value = serde_json::from_str(&files[".codex-plugin/plugin.json"]).unwrap();
        assert_eq!(manifest["name"], PLUGIN_NAME);
        assert_eq!(manifest["version"], VERSION);
        let marketplace: Value =
            serde_json::from_str(&files[".agents/plugins/marketplace.json"]).unwrap();
        assert_eq!(marketplace["name"], MARKETPLACE_NAME);
        assert_eq!(marketplace["plugins"][0]["source"], "./");
    }

    #[test]
    fn write_plugin_replaces_what_the_directory_held() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("commands")).unwrap();
        fs::write(root.join("commands").join("summary.md"), "old").unwrap();

        write_plugin(root, &claude()).unwrap();

        assert!(!root.join("commands").exists());
        assert!(root.join(".claude-plugin").join("plugin.json").exists());
        for name in ["resolve", "blame", "publish"] {
            assert!(
                root.join("skills").join(name).join("SKILL.md").exists(),
                "{name}"
            );
        }
        assert!(!root.join(MCP_PATH).exists());
    }

    #[test]
    fn write_mcp_json_records_the_command_and_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Map::new();
        settings.insert("default_tools_approval_mode".to_string(), json!("approve"));
        write_mcp_json(dir.path(), &["/usr/local/bin/gage", "mcp"], settings).unwrap();
        let text = fs::read_to_string(dir.path().join(MCP_PATH)).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let server = &value["mcpServers"]["gage"];
        assert_eq!(server["command"], "/usr/local/bin/gage");
        assert_eq!(server["args"][0], "mcp");
        assert_eq!(server["default_tools_approval_mode"], "approve");
    }

    /// An argument with spaces or quotes reaches the skill as one
    /// shell word.
    #[test]
    fn skill_arguments_are_shell_words() {
        let words = [
            "--description".to_string(),
            "For the blog's readers".to_string(),
            String::new(),
            "a/b:c@d".to_string(),
        ];
        assert_eq!(
            claude().skill_prompt("publish", &words),
            r"/gage:publish --description 'For the blog'\''s readers' '' a/b:c@d"
        );
        assert_eq!(codex().skill_prompt("resolve", &[]), "$gage:resolve");
        assert_eq!(
            pi().skill_prompt("resolve", &words[3..]),
            "/skill:gage-resolve a/b:c@d"
        );
    }
}
