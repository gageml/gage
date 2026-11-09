//! The "Filters" multi-select of a dialog that selects publish
//! filters.

use cliclack as cli;
use gage_registry::filter::FilterRegistry;

use crate::dialog::DialogError;

/// Multi-select over every filter with none selected. The hint shown
/// for each item is the first line of the filter's description.
pub fn prompt_filter_multiselect(
    registry: &FilterRegistry,
    required: bool,
) -> Result<Vec<String>, DialogError> {
    let defs = registry.list();
    let mut prompt = cli::multiselect("Filters").required(required);
    for (i, def) in defs.iter().enumerate() {
        let hint = def.description.lines().next().unwrap_or("").to_string();
        prompt = prompt.item(i, def.name.clone(), hint);
    }
    let indices: Vec<usize> = prompt.interact()?;
    Ok(indices
        .into_iter()
        .map(|i| {
            defs.get(i)
                .expect("selected index points into defs")
                .name
                .clone()
        })
        .collect())
}
