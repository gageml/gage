//! The guard on the scanner API level. The snapshot beside this file
//! lists what the runtime registers with Rune: every function, macro,
//! type, and field a scanner can name. A change to that list without
//! a higher `API_LEVEL` fails here, so a scanner written for the new
//! list never passes the check of a build that lacks it.
//!
//! The list is read from the runtime's source, since a Rune context
//! does not enumerate its items. It holds names only: a change to the
//! behavior or the parameters of an item under an unchanged name is
//! not seen, and raising the level for one is left to the author.
//!
//! After raising `API_LEVEL`, rewrite the snapshot:
//!
//! ```shell
//! GAGE_UPDATE_API_SNAPSHOT=1 cargo test -p gage-runtime --test api_snapshot
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use gage_registry::scanner::{API_LEVEL, API_LEVEL_OLDEST};

/// Text that marks a line registering an item with a Rune module.
const REGISTRATIONS: [&str; 11] = [
    "function_meta(",
    "macro_meta(",
    "type_meta::<",
    ".ty::<",
    ".function(",
    "associated_function(",
    "field_function(",
    ".constant(",
    "reexport(",
    "Module::with_crate",
    "with_item(",
];

/// Text that marks an attribute naming the item below it to Rune.
const ATTRIBUTES: [&str; 3] = ["#[rune::function", "#[rune::macro_", "#[rune("];

#[test]
fn the_registered_items_match_the_snapshot_for_the_api_level() {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "api_snapshot.txt"]
        .iter()
        .collect();
    let recorded = fs::read_to_string(&path).unwrap_or_default();
    let (recorded_level, recorded_items) = recorded.split_once('\n').unwrap_or(("", ""));
    let items = registered_items();
    let level = format!("api {API_LEVEL}");

    if std::env::var_os("GAGE_UPDATE_API_SNAPSHOT").is_some() {
        assert!(
            recorded_items == items || recorded_level != level,
            "the registered items changed; raise API_LEVEL before rewriting the snapshot"
        );
        fs::write(&path, format!("{level}\n{items}")).unwrap();
        return;
    }
    assert_eq!(
        recorded_level, level,
        "API_LEVEL changed; rewrite the snapshot with GAGE_UPDATE_API_SNAPSHOT=1"
    );
    assert!(
        recorded_items == items,
        "the items the runtime registers with Rune changed; raise API_LEVEL, and \
         API_LEVEL_OLDEST too when scanners written for the old list no longer run, then \
         rewrite the snapshot with GAGE_UPDATE_API_SNAPSHOT=1\n{}",
        difference(recorded_items, &items)
    );
}

#[test]
fn the_oldest_supported_level_is_not_above_the_current_one() {
    assert!((1..=API_LEVEL).contains(&API_LEVEL_OLDEST));
}

/// One line per registered item, `<file>: <text>`, sorted.
fn registered_items() -> String {
    let src: PathBuf = [env!("CARGO_MANIFEST_DIR"), "src"].iter().collect();
    let mut files: Vec<PathBuf> = fs::read_dir(&src)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    files.sort();
    let mut items: Vec<String> = Vec::new();
    for file in files {
        let name = file.file_name().unwrap().to_string_lossy().to_string();
        for item in file_items(&file) {
            items.push(format!("{name}: {item}\n"));
        }
    }
    items.sort();
    items.concat()
}

/// The registered items of one source file, up to its test module: a
/// registration call as written, and an attribute with the name of the
/// item it is on.
fn file_items(file: &Path) -> Vec<String> {
    let text = fs::read_to_string(file).unwrap();
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .take_while(|line| *line != "mod tests {")
        .collect();
    let mut items = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("//") {
            continue;
        }
        if ATTRIBUTES.iter().any(|a| line.starts_with(a)) {
            let item = lines
                .iter()
                .skip(i + 1)
                .find(|l| !l.starts_with('#') && !l.starts_with("//"))
                .map_or("", |l| item_head(l));
            items.push(format!("{line} {item}"));
        } else if REGISTRATIONS.iter().any(|r| line.contains(r)) {
            items.push(line.to_string());
        }
    }
    items
}

/// The start of an item's first line, through its name.
fn item_head(line: &str) -> &str {
    let end = line.find(['(', '{', '<', ':', ';']).unwrap_or(line.len());
    line[..end].trim()
}

/// The lines only one of the two lists holds.
fn difference(recorded: &str, current: &str) -> String {
    let removed = recorded
        .lines()
        .filter(|line| !current.lines().any(|l| l == *line))
        .map(|line| format!("- {line}\n"));
    let added = current
        .lines()
        .filter(|line| !recorded.lines().any(|l| l == *line))
        .map(|line| format!("+ {line}\n"));
    removed.chain(added).collect()
}
