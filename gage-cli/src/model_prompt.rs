//! Shared "Model" prompt used by scan and resolve dialogs.

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

/// Prompt for a scan model. A plain "Default" entry sits at the top
/// and is preselected; selecting it returns `None`, signaling that no
/// model should be passed and each scanner should use its own
/// per-size-tier default.
pub fn prompt_model_for_scan() -> Result<Option<String>, DialogError> {
    let mut select = cli::select("Model");
    select = select.item(DEFAULT.into(), "Default", "specified by scanners");
    for (value, label, hint) in scan_items() {
        select = select.item(value.into(), label, hint);
    }
    let model = select
        .item(CUSTOM.into(), "Other", "enter a model name")
        .initial_value(DEFAULT.into())
        .interact()?;
    if model == DEFAULT {
        return Ok(None);
    }
    if model == CUSTOM {
        let custom: String = cli::input("Model")
            .placeholder("e.g. claude-sonnet-4-6")
            .interact()?;
        return Ok(Some(custom));
    }
    Ok(Some(model))
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

fn scan_items() -> [(&'static str, &'static str, &'static str); 6] {
    let (sonnet, opus, opusplan, sonnet_1m, opus_1m, fable) = model_entries();
    [sonnet, opus, opusplan, sonnet_1m, opus_1m, fable]
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

/// Select sentinels for the model prompts.
const CUSTOM: &str = "\0custom";
const DEFAULT: &str = "\0default";
