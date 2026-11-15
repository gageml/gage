//! Model resolution for values passed to `claude --model`. A size
//! name (`small`/`medium`/`large`) resolves to a model id. The
//! family aliases claude documents (`haiku`/`sonnet`/`opus`) track
//! the latest release of each family, and the 5.x releases refuse
//! routine Gage prompts through the reasoning_extraction safety
//! classifier, so each size defaults to the latest 4.x release of its
//! family. Family aliases and concrete model ids pass through
//! unchanged.

use gage_session::ModelSizes;

/// Size used when a spec does not set a model. Every child claude
/// gets `--model`; an unset model never falls through to the user's
/// Claude Code default.
pub const DEFAULT_MODEL: &str = "medium";

/// The model each size resolves to when the user's config sets none.
pub fn default_sizes() -> ModelSizes {
    ModelSizes {
        small: Some("claude-haiku-4-5".to_string()),
        medium: Some("claude-sonnet-4-6".to_string()),
        large: Some("claude-opus-4-8".to_string()),
    }
}

/// Resolve a spec's optional model to the `--model` value under
/// `sizes`: the model of the size it names, or of [`DEFAULT_MODEL`]
/// when unset.
pub fn resolved_model(model: Option<&str>, sizes: &ModelSizes) -> String {
    resolve_model(model.unwrap_or(DEFAULT_MODEL), sizes).to_string()
}

/// The model `input` names under `sizes`: the size's model when
/// `input` is a size name, else `input` as given.
pub fn resolve_model<'a>(input: &'a str, sizes: &'a ModelSizes) -> &'a str {
    sizes.get(input).unwrap_or(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_sizes_to_their_defaults() {
        let sizes = default_sizes();
        assert_eq!(resolve_model("large", &sizes), "claude-opus-4-8");
        assert_eq!(resolve_model("medium", &sizes), "claude-sonnet-4-6");
        assert_eq!(resolve_model("small", &sizes), "claude-haiku-4-5");
    }

    #[test]
    fn passes_through_family_aliases_and_ids() {
        let sizes = default_sizes();
        for model in [
            "opus",
            "sonnet",
            "haiku",
            "claude-opus-4-8",
            "unknown-model",
        ] {
            assert_eq!(resolve_model(model, &sizes), model);
        }
    }

    #[test]
    fn default_model_when_unset() {
        let sizes = default_sizes();
        assert_eq!(resolved_model(None, &sizes), "claude-sonnet-4-6");
        assert_eq!(resolved_model(Some("large"), &sizes), "claude-opus-4-8");
        assert_eq!(
            resolved_model(Some("claude-haiku-4-5"), &sizes),
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn an_overridden_size_resolves_to_its_override_set_or_unset() {
        let sizes = default_sizes().overridden_by(&ModelSizes {
            medium: Some("claude-sonnet-5-5".to_string()),
            ..ModelSizes::default()
        });
        assert_eq!(resolved_model(None, &sizes), "claude-sonnet-5-5");
        assert_eq!(resolved_model(Some("medium"), &sizes), "claude-sonnet-5-5");
        assert_eq!(resolved_model(Some("small"), &sizes), "claude-haiku-4-5");
        assert_eq!(resolved_model(Some("sonnet"), &sizes), "sonnet");
    }
}
