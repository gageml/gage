//! The "Model" prompt of the resolve dialog.

use cliclack as cli;

use crate::dialog::DialogError;

/// The "Default" option shown at the top of the resolve Model prompt.
/// The caller supplies the model that would be used if the user did
/// not pick a model explicitly, plus a short note describing where
/// that default came from.
pub struct DefaultModel {
    pub model: String,
    pub note: &'static str,
}

/// Prompt for a `claude --model` value, ordered from best to worst.
/// Used by `gage resolve` where a quality ranking helps the user pick.
/// When `default` is provided, a "Default (MODEL)" entry sits at the
/// top and is preselected.
pub fn prompt_model_ranked(default: Option<DefaultModel>) -> Result<String, DialogError> {
    let mut select = cli::select("Model");
    let initial = default
        .as_ref()
        .map(|d| d.model.clone())
        .unwrap_or_else(|| "sonnet".into());
    if let Some(d) = &default {
        select = select.item(d.model.clone(), format!("Default ({})", d.model), d.note);
    }
    for (value, label, hint) in ranked_items() {
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

fn ranked_items() -> [(&'static str, &'static str, &'static str); 6] {
    let (sonnet, opus, opusplan, sonnet_1m, opus_1m, fable) = model_entries();
    [fable, opus_1m, opus, opusplan, sonnet_1m, sonnet]
}

type Entry = (&'static str, &'static str, &'static str);

fn model_entries() -> (Entry, Entry, Entry, Entry, Entry, Entry) {
    (
        ("sonnet", "Sonnet", "latest Sonnet"),
        ("opus", "Opus", "latest Opus"),
        (
            "opusplan",
            "Opus plan",
            "Opus for planning, Sonnet for execution",
        ),
        ("sonnet[1m]", "Sonnet (1M context)", ""),
        ("opus[1m]", "Opus (1M context)", ""),
        ("fable", "Fable", ""),
    )
}

/// Select sentinel for the custom entry of the model prompt.
const CUSTOM: &str = "\0custom";
