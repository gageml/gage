//! Model resolution for values passed to `codex exec --model`. A size
//! name (`small`/`medium`/`large`) resolves to a model slug; any other
//! name passes through unchanged. The sizes are fixed defaults that
//! the user's config can replace. Which models an account can use
//! depends on its ChatGPT plan, and nothing here asks: a model the
//! plan lacks fails when the agent starts, with Codex's own message.

use gage_session::{ModelChoice, ModelChoices, ModelSizes};

/// Size used when a spec does not set a model. Every child codex gets
/// `--model`; an unset model never falls through to the user's Codex
/// default.
pub(crate) const DEFAULT_MODEL: &str = "medium";

/// The model each size resolves to when the user's config sets none.
pub(crate) fn default_sizes() -> ModelSizes {
    ModelSizes {
        small: Some("gpt-6-luna".to_string()),
        medium: Some("gpt-6.1-terra".to_string()),
        large: Some("gpt-6.1-sol".to_string()),
    }
}

/// The models a dialog offers for a Codex session: the default sizes,
/// largest first.
pub(crate) fn model_choices() -> ModelChoices {
    let sizes = default_sizes();
    let choice = |size: &str, label: &str| {
        let model = sizes.get(size).unwrap_or(size);
        ModelChoice::new(model, label, &format!("the {size} model"))
    };
    ModelChoices {
        choices: vec![
            choice("large", "GPT-6.1 Sol"),
            choice("medium", "GPT-6.1 Terra"),
            choice("small", "GPT-6 Luna"),
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
        assert_eq!(resolved_model(Some("small"), &sizes), "gpt-6-luna");
        assert_eq!(resolved_model(Some("large"), &sizes), "gpt-6.1-sol");
        assert_eq!(
            resolved_model(Some("gpt-5.6-terra"), &sizes),
            "gpt-5.6-terra"
        );
    }

    #[test]
    fn an_unset_model_is_the_medium_size() {
        let sizes = default_sizes();
        assert_eq!(resolved_model(None, &sizes), "gpt-6.1-terra");
        let overridden = sizes.overridden_by(&ModelSizes {
            medium: Some("gpt-5.6-terra".to_string()),
            ..ModelSizes::default()
        });
        assert_eq!(resolved_model(None, &overridden), "gpt-5.6-terra");
    }

    #[test]
    fn the_dialog_offers_the_default_sizes() {
        let choices = model_choices();
        assert_eq!(choices.choices.len(), 3);
        assert_eq!(choices.example.as_deref(), Some("gpt-6.1-terra"));
    }
}
