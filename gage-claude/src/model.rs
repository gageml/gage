//! Model alias resolution for values passed to `claude --model`.
//! Maps size aliases (`small`/`medium`/`large`) to pinned model ids.
//! The family aliases claude documents (`haiku`/`sonnet`/`opus`)
//! track the latest release of each family, and the 5.x releases
//! refuse routine Gage prompts through the reasoning_extraction
//! safety classifier, so each size pins the latest 4.x release of its
//! family. Family aliases and concrete model ids pass through
//! unchanged.

use std::collections::HashMap;

const MODEL_ALIASES: &[(&str, &str)] = &[
    ("large", "claude-opus-4-8"),
    ("medium", "claude-sonnet-4-6"),
    ("small", "claude-haiku-4-5"),
];

/// Model used when a spec does not set one. Every child claude gets
/// `--model`; an unset model never falls through to the user's
/// Claude Code default.
pub const DEFAULT_MODEL: &str = "medium";

/// Resolve a spec's optional model to the `--model` value: the
/// resolved alias, or the resolved [`DEFAULT_MODEL`] when unset.
pub fn resolved_model(model: Option<&str>) -> String {
    resolve_model(model.unwrap_or(DEFAULT_MODEL)).to_string()
}

/// Per-size model overrides, built from repeated `[SIZE=]MODEL`
/// values (the CLI's `--model` option). An empty map resolves
/// identically to [`resolved_model`].
#[derive(Debug, Clone, Default)]
pub struct ModelMap(HashMap<String, String>);

impl ModelMap {
    /// Parse repeated `[SIZE=]MODEL` values. A bare MODEL sets every
    /// size alias; `SIZE=MODEL` sets one. Later values override
    /// earlier ones per size.
    pub fn from_args(values: &[String]) -> Result<Self, String> {
        let mut map = HashMap::new();
        for value in values {
            match value.split_once('=') {
                Some((size, model)) => {
                    if !MODEL_ALIASES.iter().any(|(alias, _)| *alias == size) {
                        return Err(format!(
                            "unknown model size {size:?} in {value:?}: \
                             expected small, medium, or large"
                        ));
                    }
                    if model.is_empty() {
                        return Err(format!("missing model in {value:?}"));
                    }
                    map.insert(size.to_string(), model.to_string());
                }
                None => {
                    if value.is_empty() {
                        return Err("empty model".to_string());
                    }
                    for (alias, _) in MODEL_ALIASES {
                        map.insert((*alias).to_string(), value.clone());
                    }
                }
            }
        }
        Ok(Self(map))
    }

    /// Resolve a spec's optional model like [`resolved_model`], with
    /// this map's overrides applied first. Only size aliases are
    /// remapped; family aliases and concrete ids pass through.
    pub fn resolve(&self, model: Option<&str>) -> String {
        let input = model.unwrap_or(DEFAULT_MODEL);
        let mapped = self.0.get(input).map(String::as_str).unwrap_or(input);
        resolve_model(mapped).to_string()
    }
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
        assert_eq!(resolve_model("large"), "claude-opus-4-8");
        assert_eq!(resolve_model("medium"), "claude-sonnet-4-6");
        assert_eq!(resolve_model("small"), "claude-haiku-4-5");
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
        assert_eq!(resolved_model(None), "claude-sonnet-4-6");
        assert_eq!(resolved_model(Some("large")), "claude-opus-4-8");
        assert_eq!(resolved_model(Some("claude-haiku-4-5")), "claude-haiku-4-5");
    }

    fn map(values: &[&str]) -> ModelMap {
        let owned: Vec<String> = values.iter().map(|s| s.to_string()).collect();
        ModelMap::from_args(&owned).unwrap()
    }

    #[test]
    fn empty_map_matches_resolved_model() {
        let m = map(&[]);
        assert_eq!(m.resolve(None), "claude-sonnet-4-6");
        assert_eq!(m.resolve(Some("large")), "claude-opus-4-8");
        assert_eq!(m.resolve(Some("claude-haiku-4-5")), "claude-haiku-4-5");
    }

    #[test]
    fn bare_model_sets_every_size() {
        let m = map(&["claude-sonnet-4-6"]);
        assert_eq!(m.resolve(Some("small")), "claude-sonnet-4-6");
        assert_eq!(m.resolve(Some("medium")), "claude-sonnet-4-6");
        assert_eq!(m.resolve(Some("large")), "claude-sonnet-4-6");
        assert_eq!(m.resolve(None), "claude-sonnet-4-6");
    }

    #[test]
    fn scoped_overrides_bare_and_last_wins() {
        let m = map(&["claude-sonnet-4-6", "large=claude-opus-4-7", "large=opus"]);
        assert_eq!(m.resolve(Some("medium")), "claude-sonnet-4-6");
        assert_eq!(m.resolve(Some("large")), "opus");
    }

    #[test]
    fn map_leaves_non_size_inputs_alone() {
        let m = map(&["claude-sonnet-4-6"]);
        assert_eq!(m.resolve(Some("sonnet")), "sonnet");
        assert_eq!(m.resolve(Some("claude-opus-4-7")), "claude-opus-4-7");
    }

    #[test]
    fn rejects_bad_values() {
        for bad in ["huge=opus", "medium=", ""] {
            assert!(ModelMap::from_args(&[bad.to_string()]).is_err(), "{bad:?}");
        }
    }
}
