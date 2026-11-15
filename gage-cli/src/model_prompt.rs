//! The "Model" step and prompt of a dialog that starts a harness
//! session.

use std::sync::Arc;

use cliclack as cli;
use console::style;
use gage_session::{Driver, ModelChoices};

use crate::dialog::DialogError;
use crate::source::driver_registry;

/// The "Model" step of a dialog that starts a harness session for
/// `command`: the given model is shown, `yes` without one takes the
/// harness's default, and otherwise the prompt asks, offering the
/// models of the driver the session starts on. `None` is the default
/// model.
pub fn model_step(
    command: &str,
    given: Option<&str>,
    yes: bool,
) -> Result<Option<String>, DialogError> {
    match given {
        Some(model) => {
            cli::log::step(format!("Model\n{}", style(model).dim()))?;
            Ok(Some(model.to_string()))
        }
        None if yes => {
            cli::log::step(format!("Model\n{}", style("Default").dim()))?;
            Ok(None)
        }
        None => prompt_model(&*session_driver(command)?),
    }
}

/// The driver a skill session starts on: the default driver.
fn session_driver(command: &str) -> Result<Arc<dyn Driver>, DialogError> {
    driver_registry().default().ok_or_else(|| {
        DialogError::Failed(format!("{command}: no default session driver registered"))
    })
}

/// Prompt for a model among those `driver` offers, best first, where
/// a quality ranking helps the user pick. The preselected "Default"
/// entry returns `None`: the harness's configured model, as the
/// driver resolves it. A driver that offers no models gets the name
/// prompt alone.
fn prompt_model(driver: &dyn Driver) -> Result<Option<String>, DialogError> {
    let offered = driver.model_choices();
    if offered.choices.is_empty() {
        return prompt_model_name(&offered, false);
    }
    let mut select = cli::select("Model");
    for (value, label, hint) in select_items(&offered) {
        select = select.item(value, label, hint);
    }
    let model = select.initial_value(DEFAULT.to_string()).interact()?;
    if model == DEFAULT {
        return Ok(None);
    }
    if model == CUSTOM {
        return prompt_model_name(&offered, true);
    }
    Ok(Some(model))
}

/// The select's entries as `(value, label, hint)`: the default, then
/// the driver's models in its order, then the entry that asks for a
/// name.
fn select_items(offered: &ModelChoices) -> Vec<(String, String, String)> {
    let default = (
        DEFAULT.to_string(),
        "Default".to_string(),
        "the model the harness is configured with".to_string(),
    );
    let custom = (
        CUSTOM.to_string(),
        "Other".to_string(),
        "enter a model name".to_string(),
    );
    let models = offered
        .choices
        .iter()
        .map(|c| (c.model.clone(), c.label.clone(), c.hint.clone()));
    [default]
        .into_iter()
        .chain(models)
        .chain([custom])
        .collect()
}

/// Ask for a model by name. When a name is not `required`, an empty
/// answer is the default model.
fn prompt_model_name(
    offered: &ModelChoices,
    required: bool,
) -> Result<Option<String>, DialogError> {
    let mut input = cli::input("Model").required(required);
    if let Some(placeholder) = name_placeholder(offered, required) {
        input = input.placeholder(&placeholder);
    }
    let name: String = input.interact()?;
    let name = name.trim();
    Ok((!name.is_empty()).then(|| name.to_string()))
}

/// The hint the name prompt shows before anything is typed.
fn name_placeholder(offered: &ModelChoices, required: bool) -> Option<String> {
    let example = offered.example.as_ref().map(|e| format!("e.g. {e}"));
    if required {
        return example;
    }
    Some(match example {
        Some(example) => format!("{example}; empty for the default"),
        None => "empty for the default".to_string(),
    })
}

/// Select sentinel for the custom entry of the model prompt.
const CUSTOM: &str = "\0custom";
/// Select sentinel for the default entry of the model prompt.
const DEFAULT: &str = "\0default";

#[cfg(test)]
mod tests {
    use gage_session::ModelChoice;

    use super::*;

    #[test]
    fn select_offers_the_drivers_models_between_default_and_other() {
        let offered = ModelChoices {
            choices: vec![
                ModelChoice::new("big", "Big", "best"),
                ModelChoice::new("little", "Little", ""),
            ],
            example: Some("vendor-big-1".to_string()),
        };
        let items = select_items(&offered);
        let values: Vec<&str> = items.iter().map(|(value, _, _)| value.as_str()).collect();
        assert_eq!(values, [DEFAULT, "big", "little", CUSTOM]);
        assert_eq!(items[1], ("big".into(), "Big".into(), "best".into()));
        assert_eq!(
            name_placeholder(&offered, true).as_deref(),
            Some("e.g. vendor-big-1")
        );
    }

    #[test]
    fn a_driver_with_no_models_is_asked_by_name_with_empty_for_default() {
        let none = ModelChoices::default();
        assert_eq!(name_placeholder(&none, true), None);
        assert_eq!(
            name_placeholder(&none, false).as_deref(),
            Some("empty for the default")
        );
        let example_only = ModelChoices {
            choices: Vec::new(),
            example: Some("vendor-big-1".to_string()),
        };
        assert_eq!(
            name_placeholder(&example_only, false).as_deref(),
            Some("e.g. vendor-big-1; empty for the default")
        );
    }

    #[test]
    fn claude_offers_its_models_best_first() {
        let driver = driver_registry().default().unwrap();
        let items = select_items(&driver.model_choices());
        let labels: Vec<&str> = items.iter().map(|(_, label, _)| label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Default",
                "Fable",
                "Opus (1M context)",
                "Opus",
                "Opus plan",
                "Sonnet (1M context)",
                "Sonnet",
                "Other"
            ]
        );
    }
}
