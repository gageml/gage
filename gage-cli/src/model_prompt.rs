//! Shared "Model" prompt used by scan and resolve dialogs.

use cliclack as cli;

use crate::dialog::DialogError;

/// The "Default" option shown at the top of the Model prompt. The
/// caller supplies the model that would be used if the user did not
/// pick a model explicitly, plus a short note describing where that
/// default came from.
pub struct DefaultModel {
    pub model: String,
    pub note: &'static str,
}

/// Prompt for a `claude --model` value. When `default` is provided, a
/// "Default (MODEL)" entry sits at the top and is preselected.
pub fn prompt_model(default: Option<DefaultModel>) -> Result<String, DialogError> {
    let mut select = cli::select("Model");
    let initial = default
        .as_ref()
        .map(|d| d.model.clone())
        .unwrap_or_else(|| "sonnet".into());
    if let Some(d) = &default {
        select = select.item(d.model.clone(), format!("Default ({})", d.model), d.note);
    }
    let model = select
        .item("sonnet".into(), "Sonnet", "latest Sonnet")
        .item("opus".into(), "Opus", "latest Opus")
        .item(
            "opusplan".into(),
            "Opus plan",
            "Opus for planning, Sonnet for execution",
        )
        .item("sonnet[1m]".into(), "Sonnet (1M context)", "")
        .item("opus[1m]".into(), "Opus (1M context)", "")
        .item("fable".into(), "Fable", "")
        .item(
            "best".into(),
            "Best",
            "Fable where available, otherwise latest Opus",
        )
        .item(CUSTOM.into(), "Other", "enter a model name")
        .initial_value(initial)
        .interact()?;
    if model == CUSTOM {
        let custom: String = cli::input("Model")
            .placeholder("e.g. claude-sonnet-4-6")
            .interact()?;
        return Ok(custom);
    }
    Ok(model)
}

/// Select sentinel for the free-form model prompt.
const CUSTOM: &str = "\0custom";
