//! TEXT option values: the text as given, or the contents of a file
//! when given as `@PATH`.

use std::fs;
use std::path::Path;

/// The text an option holds, or the contents of PATH when `value` is
/// `@PATH`. A missing or unreadable file prints `command: option
/// path: error` and exits.
pub fn text_value(command: &str, option: &str, value: &str) -> String {
    match value.strip_prefix('@') {
        Some(path) => read_text_file(command, option, Path::new(path)),
        None => value.to_string(),
    }
}

fn read_text_file(command: &str, option: &str, path: &Path) -> String {
    match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("{command}: {option} {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}
