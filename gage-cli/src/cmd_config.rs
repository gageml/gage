use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, Subcommand};
use tabled::{
    Table,
    settings::{Color, Style, object::Rows},
};
use toml::Value;

use gage_core::config::{
    Config, discover_config_paths, display_user_config_path, find_project_gage_dir,
    find_project_root, user_config_path,
};

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Show effective Gage configuration
    Show,

    /// Edit a Gage config file in the system editor
    Edit(EditArgs),
}

#[derive(Args)]
pub struct EditArgs {
    /// Edit the user config at `~/.gage/config.toml`
    #[arg(long, conflicts_with_all = ["project", "path"])]
    user: bool,

    /// Edit the nearest project `.gage/config.toml`
    #[arg(long, conflicts_with_all = ["user", "path"])]
    project: bool,

    /// Edit `<path>/.gage/config.toml`
    #[arg(long, conflicts_with_all = ["user", "project"], value_name = "DIR")]
    path: Option<PathBuf>,
}

pub fn run(command: ConfigCommand) {
    match command {
        ConfigCommand::Show => show(),
        ConfigCommand::Edit(args) => edit(args),
    }
}

fn show() {
    let cwd = match env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error reading cwd: {e}");
            std::process::exit(1);
        }
    };
    let paths = discover_config_paths(&cwd);

    // Inner-to-outer collection of (setting, source, raw value).
    let mut occurrences: Vec<(String, PathBuf, Value)> = Vec::new();
    for path in &paths {
        let content = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error reading {}: {e}", path.display());
                std::process::exit(1);
            }
        };
        let table: toml::Table = match toml::from_str(&content) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Error parsing {}: {e}", path.display());
                std::process::exit(1);
            }
        };
        flatten_into(&mut occurrences, path, "", &table);
    }

    if occurrences.is_empty() {
        println!("\x1b[2;3mGage currently has no configured settings\x1b[22;23m");
        return;
    }

    // Keep only the inner-most occurrence per setting (VS Code semantics:
    // scalars and arrays are replaced; tables deep-merge naturally because
    // we flatten to dotted leaf paths).
    let mut order: Vec<String> = Vec::new();
    let mut winners: BTreeMap<String, (PathBuf, Value)> = BTreeMap::new();
    for (key, path, value) in occurrences {
        if let Entry::Vacant(e) = winners.entry(key.clone()) {
            order.push(key);
            e.insert((path, value));
        }
    }

    let mut rows: Vec<Vec<String>> = Vec::new();
    rows.push(vec![
        "Setting".to_string(),
        "Value".to_string(),
        "Source".to_string(),
    ]);
    for key in order {
        let (path, value) = winners
            .get(&key)
            .expect("key recorded in `order` is present in `winners`");
        rows.push(vec![key, render_value(value), display_source(path)]);
    }

    let table = Table::from_iter(rows)
        .with(Style::rounded())
        .modify(Rows::first(), Color::FG_BRIGHT_YELLOW)
        .to_string();
    println!("{table}");
}

/// Recursively flattens nested TOML tables into dotted keys, appending
/// `(key, path, value)` entries in document order. Leaf = anything that
/// isn't a table.
fn flatten_into(
    out: &mut Vec<(String, PathBuf, Value)>,
    path: &Path,
    prefix: &str,
    table: &toml::Table,
) {
    for (k, v) in table {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            Value::Table(inner) => flatten_into(out, path, &key, inner),
            _ => out.push((key, path.to_path_buf(), v.clone())),
        }
    }
}

/// Renders a TOML value as it would appear inline in a config file
/// (e.g. `"evidence"`, `["a", "b"]`, `42`, `true`).
fn render_value(v: &Value) -> String {
    v.to_string()
}

fn display_source(path: &Path) -> String {
    let user = user_config_path();
    if path == user {
        return display_user_config_path();
    }
    if let Ok(home) = env::var("HOME")
        && let Ok(rel) = path.strip_prefix(&home)
    {
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}

fn edit(args: EditArgs) {
    let target = match resolve_edit_target(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    if let Some(parent) = target.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("Error creating {}: {e}", parent.display());
        std::process::exit(1);
    }
    if !target.exists()
        && let Err(e) = Config::default().save_to(&target)
    {
        eprintln!("Error initializing {}: {e}", target.display());
        std::process::exit(1);
    }

    let editor = env::var("VISUAL")
        .or_else(|_| env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());

    let status = Command::new(&editor).arg(&target).status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("Editor exited with status {s}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Failed to launch editor `{editor}`: {e}");
            std::process::exit(1);
        }
    }
}

fn resolve_edit_target(args: &EditArgs) -> Result<PathBuf, String> {
    if args.user {
        return Ok(user_config_path());
    }
    if let Some(p) = &args.path {
        return Ok(p.join(".gage").join("config.toml"));
    }
    if args.project {
        let cwd = env::current_dir().map_err(|e| format!("Error reading cwd: {e}"))?;
        if let Some(dir) = find_project_gage_dir(&cwd) {
            return Ok(dir.join(".gage").join("config.toml"));
        }
        if let Some(dir) = find_project_root(&cwd) {
            return Ok(dir.join(".gage").join("config.toml"));
        }
        return Err(
            "No project directory found. Use `gage config edit --path .` to edit the config in the current directory."
                .to_string(),
        );
    }
    Err("One of --user, --project, or --path is required".to_string())
}
