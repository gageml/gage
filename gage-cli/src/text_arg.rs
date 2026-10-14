//! TEXT option values: the text as given, or the contents of a file
//! when given as `@PATH`.

use std::fs;
use std::path::Path;

use anyhow::Context;

/// The text an option holds, or the contents of PATH when `value` is
/// `@PATH`. A missing or unreadable file fails as `command: option
/// path: error`.
pub fn text_value(command: &str, option: &str, value: &str) -> anyhow::Result<String> {
    match value.strip_prefix('@') {
        Some(path) => read_text_file(command, option, Path::new(path)),
        None => Ok(value.to_string()),
    }
}

fn read_text_file(command: &str, option: &str, path: &Path) -> anyhow::Result<String> {
    fs::read_to_string(path).with_context(|| format!("{command}: {option} {}", path.display()))
}
