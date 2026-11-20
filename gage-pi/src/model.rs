//! Model resolution for values passed to `pi --model`. A size name
//! (`small`/`medium`/`large`) resolves to a model; any other name
//! passes through unchanged. Pi names a model as `<provider>/<id>`
//! and reaches many providers, so the sizes are defaults for one
//! provider that the user's config replaces for another. Which models
//! an account can use is the provider's to say, and nothing here
//! asks: a model the account lacks fails when the agent starts, with
//! Pi's own message.

use gage_session::{ModelChoice, ModelChoices, ModelSizes};

/// Size used when a spec does not set a model. Every agent child gets
/// `--model`; an unset model never falls through to the user's Pi
/// default.
pub(crate) const DEFAULT_MODEL: &str = "medium";

/// The model each size resolves to when the user's config sets none.
pub(crate) fn default_sizes() -> ModelSizes {
    ModelSizes {
        small: Some("anthropic/claude-haiku-4-5".to_string()),
        medium: Some("anthropic/claude-sonnet-4-6".to_string()),
        large: Some("anthropic/claude-opus-4-8".to_string()),
    }
}

/// The models a dialog offers for a Pi session: the default sizes,
/// largest first.
pub(crate) fn model_choices() -> ModelChoices {
    let sizes = default_sizes();
    let choice = |size: &str, label: &str| {
        let model = sizes.get(size).unwrap_or(size);
        ModelChoice::new(model, label, &format!("the {size} model"))
    };
    ModelChoices {
        choices: vec![
            choice("large", "Claude Opus 4.8"),
            choice("medium", "Claude Sonnet 4.6"),
            choice("small", "Claude Haiku 4.5"),
        ],
        example: sizes.medium,
    }
}

/// Resolve a spec's optional model to the `--model` value under
/// `sizes`: the model of the size it names, or of [`DEFAULT_MODEL`]
/// when unset.
pub(crate) fn resolved_model(model: Option<&str>, sizes: &ModelSizes) -> String {
    let input = model.unwrap_or(DEFAULT_MODEL);
    sizes.get(input).unwrap_or(input).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_resolves_to_its_model_and_a_name_passes_through() {
        let sizes = default_sizes();
        assert_eq!(
            resolved_model(Some("small"), &sizes),
            "anthropic/claude-haiku-4-5"
        );
        assert_eq!(
            resolved_model(Some("large"), &sizes),
            "anthropic/claude-opus-4-8"
        );
        assert_eq!(
            resolved_model(Some("openai/gpt-6.1-terra"), &sizes),
            "openai/gpt-6.1-terra"
        );
    }

    #[test]
    fn an_unset_model_is_the_medium_size() {
        let sizes = default_sizes();
        assert_eq!(resolved_model(None, &sizes), "anthropic/claude-sonnet-4-6");
        let overridden = sizes.overridden_by(&ModelSizes {
            medium: Some("openai/gpt-6.1-terra".to_string()),
            ..ModelSizes::default()
        });
        assert_eq!(resolved_model(None, &overridden), "openai/gpt-6.1-terra");
    }

    #[test]
    fn the_dialog_offers_the_default_sizes() {
        let choices = model_choices();
        assert_eq!(choices.choices.len(), 3);
        assert_eq!(
            choices.example.as_deref(),
            Some("anthropic/claude-sonnet-4-6")
        );
    }
}
