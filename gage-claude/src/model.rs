//! Model alias resolution for values passed to `claude --model`.
//! Maps size aliases (`small`/`medium`/`large`) to the version-free
//! family aliases claude itself documents (`haiku`/`sonnet`/`opus`),
//! which track the latest release of each family. Family aliases and
//! concrete model ids pass through unchanged.

use std::fs;
use std::path::{Path, PathBuf};

const MODEL_ALIASES: &[(&str, &str)] =
    &[("large", "opus"), ("medium", "sonnet"), ("small", "haiku")];

/// Model used when a spec does not set one. Every child claude gets
/// `--model`; an unset model never falls through to the user's
/// Claude Code default.
pub const DEFAULT_MODEL: &str = "medium";

/// Resolve a spec's optional model to the `--model` value: the
/// resolved alias, or the resolved [`DEFAULT_MODEL`] when unset.
pub fn resolved_model(model: Option<&str>) -> String {
    resolve_model(model.unwrap_or(DEFAULT_MODEL)).to_string()
}

/// The `model` configured in Claude Code settings, nearest match wins.
/// Walks from `start` up through `$HOME` (or the filesystem root),
/// checking `.claude/settings.local.json` then `.claude/settings.json`
/// at each directory; the `$HOME` step covers the user scope. `None`
/// when no settings file sets a model — a missing or unparsable file
/// reads as unset.
pub fn configured_model(start: &Path) -> Option<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for dir in start.ancestors() {
        let claude = dir.join(".claude");
        for name in ["settings.local.json", "settings.json"] {
            if let Some(m) = settings_model(&claude.join(name)) {
                return Some(m);
            }
        }
        if home.as_deref() == Some(dir) {
            break;
        }
    }
    None
}

fn settings_model(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(json.get("model")?.as_str()?.to_string())
}

pub fn resolve_model(input: &str) -> &str {
    for (alias, target) in MODEL_ALIASES {
        if *alias == input {
            return target;
        }
    }
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_aliases() {
        assert_eq!(resolve_model("large"), "opus");
        assert_eq!(resolve_model("medium"), "sonnet");
        assert_eq!(resolve_model("small"), "haiku");
    }

    #[test]
    fn passes_through_family_aliases_and_ids() {
        assert_eq!(resolve_model("opus"), "opus");
        assert_eq!(resolve_model("sonnet"), "sonnet");
        assert_eq!(resolve_model("haiku"), "haiku");
        assert_eq!(resolve_model("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(resolve_model("unknown-model"), "unknown-model");
    }

    #[test]
    fn default_model_when_unset() {
        assert_eq!(resolved_model(None), "sonnet");
        assert_eq!(resolved_model(Some("large")), "opus");
        assert_eq!(resolved_model(Some("claude-haiku-4-5")), "claude-haiku-4-5");
    }
}
