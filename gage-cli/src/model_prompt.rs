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
/// "Default (MODEL)" entry sits at the top and is preselected. The
/// remaining entries follow the fixed order used by `gage scan`.
pub fn prompt_model(default: Option<DefaultModel>) -> Result<String, DialogError> {
    prompt_model_with_order(default, false)
}

/// Same as [`prompt_model`] but orders the entries from best to worst.
/// Used by `gage resolve` where a quality ranking helps the user pick.
pub fn prompt_model_ranked(default: Option<DefaultModel>) -> Result<String, DialogError> {
    prompt_model_with_order(default, true)
}

fn prompt_model_with_order(
    default: Option<DefaultModel>,
    ranked: bool,
) -> Result<String, DialogError> {
    let mut select = cli::select("Model");
    let initial = default
        .as_ref()
        .map(|d| d.model.clone())
        .unwrap_or_else(|| "sonnet".into());
    if let Some(d) = &default {
        select = select.item(d.model.clone(), format!("Default ({})", d.model), d.note);
    }
    let sonnet = ("sonnet", "Sonnet", "latest Sonnet");
    let opus = ("opus", "Opus", "latest Opus");
    let opusplan = (
        "opusplan",
        "Opus plan",
        "Opus for planning, Sonnet for execution",
    );
    let sonnet_1m = ("sonnet[1m]", "Sonnet (1M context)", "");
    let opus_1m = ("opus[1m]", "Opus (1M context)", "");
    let fable = ("fable", "Fable", "");
    let best = (
        "best",
        "Best",
        "Fable where available, otherwise latest Opus",
    );
    let items = if ranked {
        [best, fable, opus_1m, opus, opusplan, sonnet_1m, sonnet]
    } else {
        [sonnet, opus, opusplan, sonnet_1m, opus_1m, fable, best]
    };
    for (value, label, hint) in items {
        select = select.item(value.into(), label, hint);
    }
    let model = select
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
