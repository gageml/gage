//! Model resolution for values passed to `claude --model`. A size
//! name (`small`/`medium`/`large`) resolves to a model. The family
//! aliases claude documents (`haiku`/`sonnet`/`opus`) track the latest
//! release of each family. Each size defaults to the alias of one
//! family, so it follows the latest release of that family. Family
//! aliases and concrete model ids pass through unchanged.

use gage_session::{ModelChoice, ModelChoices, ModelSizes};

/// Size used when a spec does not set a model. Every child claude
/// gets `--model`; an unset model never falls through to the user's
/// Claude Code default.
pub const DEFAULT_MODEL: &str = "medium";

/// The model each size resolves to when the user's config sets none.
pub fn default_sizes() -> ModelSizes {
    ModelSizes {
        small: Some("haiku".to_string()),
        medium: Some("sonnet".to_string()),
        large: Some("opus".to_string()),
    }
}

/// The models a dialog offers for a Claude Code session, best first.
/// Each is a name Claude Code resolves itself.
pub fn model_choices() -> ModelChoices {
    let choice = ModelChoice::new;
    ModelChoices {
        choices: vec![
            choice("fable", "Fable", ""),
            choice("opus[1m]", "Opus (1M context)", ""),
            choice("opus", "Opus", "latest Opus"),
            choice(
                "opusplan",
                "Opus plan",
                "Opus for planning, Sonnet for execution",
            ),
            choice("sonnet[1m]", "Sonnet (1M context)", ""),
            choice("sonnet", "Sonnet", "latest Sonnet"),
        ],
        example: Some("claude-sonnet-4-6".to_string()),
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
    fn choices_run_best_first_and_pass_through_resolution() {
        let offered = model_choices();
        let models: Vec<&str> = offered.choices.iter().map(|c| c.model.as_str()).collect();
        assert_eq!(
            models,
            [
                "fable",
                "opus[1m]",
                "opus",
                "opusplan",
                "sonnet[1m]",
                "sonnet"
            ]
        );
        assert_eq!(offered.example.as_deref(), Some("claude-sonnet-4-6"));
        let sizes = default_sizes();
        for model in models {
            assert_eq!(resolve_model(model, &sizes), model);
        }
    }

    #[test]
    fn resolves_sizes_to_their_defaults() {
        let sizes = default_sizes();
        assert_eq!(resolve_model("large", &sizes), "opus");
        assert_eq!(resolve_model("medium", &sizes), "sonnet");
        assert_eq!(resolve_model("small", &sizes), "haiku");
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
        assert_eq!(resolved_model(None, &sizes), "sonnet");
        assert_eq!(resolved_model(Some("large"), &sizes), "opus");
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
        assert_eq!(resolved_model(Some("small"), &sizes), "haiku");
        assert_eq!(resolved_model(Some("sonnet"), &sizes), "sonnet");
    }
}
