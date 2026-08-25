use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rust_embed::RustEmbed;

use gage_core::config::gage_home;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const MARKETPLACE_PATH: &str = ".claude-plugin/marketplace.json";

#[derive(RustEmbed)]
#[folder = "config/"]
struct PluginFiles;

/// Returns the ephemeral plugin directory: `~/.gage/tmp/claude`.
pub fn plugin_dir() -> PathBuf {
    gage_home().join("tmp").join("claude")
}

/// Replace `%VAR%` placeholders in a template string.
fn expand_vars(template: &str, gage_bin: &Path) -> String {
    template
        .replace("%VERSION%", VERSION)
        .replace("%GAGE_BIN%", &gage_bin.to_string_lossy())
}

/// Write plugin files to `root`, wiring the embedded `plugin.json` to
/// invoke `gage_bin` for the MCP server.
///
/// Removes any existing contents at `root` first to avoid stale files,
/// then materializes every file under `config/` (excluding the
/// marketplace manifest, which [`write_marketplace_manifest_to`]
/// writes separately).
pub fn write_plugin_files_to(root: &Path, gage_bin: &Path) -> io::Result<()> {
    if root.exists() {
        fs::remove_dir_all(root)?;
    }

    for path in PluginFiles::iter() {
        if path.as_ref() == MARKETPLACE_PATH {
            continue;
        }
        write_embedded(&path, root, gage_bin)?;
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
    write_embedded(MARKETPLACE_PATH, root, Path::new(""))
}

fn write_embedded(rel_path: &str, root: &Path, gage_bin: &Path) -> io::Result<()> {
    let file = PluginFiles::get(rel_path)
        .unwrap_or_else(|| panic!("embedded plugin file missing: {rel_path}"));
    let bytes = file.data.as_ref();
    let contents = match std::str::from_utf8(bytes) {
        Ok(text) => expand_vars(text, gage_bin).into_bytes(),
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
        let gage_bin = Path::new("/usr/local/bin/gage");
        write_plugin_files_to(dir.path(), gage_bin).unwrap();

        let plugin_json = dir.path().join(".claude-plugin").join("plugin.json");
        assert!(plugin_json.exists());

        let content = fs::read_to_string(&plugin_json).unwrap();
        assert!(content.contains("\"name\": \"gage\""));
        assert!(content.contains(&format!("\"version\": \"{}\"", VERSION)));
        assert!(!content.contains("%VERSION%"));

        let mcp_json = dir.path().join(".mcp.json");
        assert!(mcp_json.exists());
        let mcp_content = fs::read_to_string(&mcp_json).unwrap();
        assert!(mcp_content.contains("/usr/local/bin/gage"));
        assert!(!mcp_content.contains("%GAGE_BIN%"));

        let skill = dir.path().join("skills").join("resolve").join("SKILL.md");
        assert!(skill.exists());

        assert!(
            !dir.path()
                .join(".claude-plugin")
                .join("marketplace.json")
                .exists()
        );
    }

    #[test]
    fn write_plugin_files_cleans_stale_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let old_dir = root.join("commands");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("summary.md"), "old").unwrap();

        write_plugin_files_to(root, Path::new("/bin/gage")).unwrap();

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
}
