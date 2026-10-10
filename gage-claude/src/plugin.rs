use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rust_embed::RustEmbed;
use serde_json::json;

use gage_core::config::gage_home;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const MARKETPLACE_PATH: &str = ".claude-plugin/marketplace.json";
const MCP_PATH: &str = ".mcp.json";

#[derive(RustEmbed)]
#[folder = "config/"]
struct PluginFiles;

/// Returns the ephemeral plugin directory: `~/.gage/tmp/claude`.
pub fn plugin_dir() -> PathBuf {
    gage_home().join("tmp").join("claude")
}

/// Replace `%VAR%` placeholders in a template string.
fn expand_vars(template: &str) -> String {
    template.replace("%VERSION%", VERSION)
}

/// Write plugin files to `root`.
///
/// Removes any existing contents at `root` first to avoid stale files,
/// then materializes every embedded file under `config/` except the
/// marketplace manifest (which [`write_marketplace_manifest_to`]
/// writes separately). The MCP server registration is written
/// separately by [`write_mcp_json`] from the install command.
pub fn write_plugin_files_to(root: &Path) -> io::Result<()> {
    if root.exists() {
        fs::remove_dir_all(root)?;
    }

    for path in PluginFiles::iter() {
        if path.as_ref() == MARKETPLACE_PATH {
            continue;
        }
        write_embedded(&path, root)?;
    }

    Ok(())
}

/// Write the marketplace manifest to `root/.claude-plugin/marketplace.json`.
///
/// The marketplace has a single entry pointing at `root` itself (source
/// `.`), so the same directory serves as both the marketplace root and
/// the plugin root. Callers should invoke this alongside
/// [`write_plugin_files_to`] with the same `root`.
pub fn write_marketplace_manifest_to(root: &Path) -> io::Result<()> {
    write_embedded(MARKETPLACE_PATH, root)
}

/// Write `.mcp.json` under `root` for the given launch command.
/// `mcp_cmd[0]` is the executable; `mcp_cmd[1..]` are its arguments.
pub fn write_mcp_json(root: &Path, mcp_cmd: &[&str]) -> io::Result<()> {
    let (program, args) = mcp_cmd
        .split_first()
        .expect("mcp_cmd must name an executable");
    let doc = json!({
        "mcpServers": {
            "gage": {
                "command": program,
                "args": args,
            }
        }
    });
    let text = serde_json::to_string_pretty(&doc).expect("serde_json cannot fail on simple map");
    let dest = root.join(MCP_PATH);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dest, text)
}

fn write_embedded(rel_path: &str, root: &Path) -> io::Result<()> {
    let file = PluginFiles::get(rel_path)
        .unwrap_or_else(|| panic!("embedded plugin file missing: {rel_path}"));
    let bytes = file.data.as_ref();
    let contents = match std::str::from_utf8(bytes) {
        Ok(text) => expand_vars(text).into_bytes(),
        Err(_) => bytes.to_vec(),
    };

    let dest = root.join(rel_path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dest, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_plugin_files_creates_expected_structure() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin_files_to(dir.path()).unwrap();

        let plugin_json = dir.path().join(".claude-plugin").join("plugin.json");
        assert!(plugin_json.exists());

        let content = fs::read_to_string(&plugin_json).unwrap();
        assert!(content.contains("\"name\": \"gage\""));
        assert!(content.contains(&format!("\"version\": \"{}\"", VERSION)));
        assert!(!content.contains("%VERSION%"));

        let skill = dir.path().join("skills").join("resolve").join("SKILL.md");
        assert!(skill.exists());

        assert!(
            !dir.path()
                .join(".claude-plugin")
                .join("marketplace.json")
                .exists()
        );
        assert!(!dir.path().join(".mcp.json").exists());
    }

    #[test]
    fn write_plugin_files_cleans_stale_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let old_dir = root.join("commands");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("summary.md"), "old").unwrap();

        write_plugin_files_to(root).unwrap();

        assert!(!root.join("commands").exists());
        assert!(root.join(".claude-plugin").join("plugin.json").exists());
    }

    #[test]
    fn write_marketplace_manifest_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        write_marketplace_manifest_to(dir.path()).unwrap();

        let path = dir.path().join(".claude-plugin").join("marketplace.json");
        assert!(path.exists());

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"name\": \"gage\""));
        assert!(content.contains("\"source\": \"./\""));
    }

    #[test]
    fn write_mcp_json_records_the_command() {
        let dir = tempfile::tempdir().unwrap();
        write_mcp_json(dir.path(), &["/usr/local/bin/gage", "mcp2"]).unwrap();
        let text = fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["mcpServers"]["gage"]["command"].as_str(),
            Some("/usr/local/bin/gage")
        );
        assert_eq!(
            value["mcpServers"]["gage"]["args"][0].as_str(),
            Some("mcp2")
        );
    }
}
