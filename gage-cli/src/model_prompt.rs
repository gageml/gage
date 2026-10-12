//! The "Model" prompt of the resolve dialog.

use cliclack as cli;

use crate::dialog::DialogError;

/// Prompt for a model, ordered from best to worst. Used by `gage
/// resolve` where a quality ranking helps the user pick. The
/// preselected "Default" entry returns `None`: the harness's
/// configured model, as the driver resolves it.
pub fn prompt_model_ranked() -> Result<Option<String>, DialogError> {
    let mut select = cli::select("Model").item(
        DEFAULT.into(),
        "Default",
        "the model the harness is configured with",
    );
    for (value, label, hint) in ranked_items() {
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
/// Select sentinel for the default entry of the model prompt.
const DEFAULT: &str = "\0default";
