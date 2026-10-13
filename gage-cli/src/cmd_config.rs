use std::env;
use std::process::Command;

use clap::Subcommand;
use toml::{Table, Value};

use gage_core::config::{Config, user_config_path};

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// List effective configuration
    ///
    /// List effective configuration. Every setting is printed as
    /// "name = value" with its working value, which is the value in
    /// ~/.gage/config.toml or the default when the file does not set
    /// it.
    List,

    /// Show effective value of a setting
    ///
    /// Show effective value of a setting. NAME is dotted, as printed by
    /// "gage config list". A string prints without quotes; any other
    /// value prints as it appears in config.toml.
    Get {
        /// Setting to show
        name: String,
    },

    /// Edit config file in system editor
    ///
    /// Edit config file in system editor. Opens ~/.gage/config.toml in
    /// $VISUAL or $EDITOR, creating it with default values if absent.
    Edit,
}

pub fn run(command: ConfigCommand) {
    match command {
        ConfigCommand::List => list(),
        ConfigCommand::Get { name } => get(&name),
        ConfigCommand::Edit => edit(),
    }
}

fn list() {
    let effective = load_effective("list");
    for (name, value) in leaves(&effective) {
        println!("{name} = {value}");
    }
}

fn get(name: &str) {
    let effective = load_effective("get");
    let Some(value) = leaves(&effective).into_iter().find(|(n, _)| n == name) else {
        eprintln!("gage config get: no setting named {name}");
        std::process::exit(1);
    };
    match value.1 {
        Value::String(s) => println!("{s}"),
        other => println!("{other}"),
    }
}

/// Loads the user config and converts it to a TOML table so that
/// every setting, set or defaulted, has a dotted name and a value.
fn load_effective(verb: &str) -> Table {
    let config = match Config::load_user() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gage config {verb}: {e}");
            std::process::exit(1);
        }
    };
    match Value::try_from(&config) {
        Ok(Value::Table(t)) => t,
        Ok(_) => unreachable!("Config serializes to a TOML table"),
        Err(e) => {
            eprintln!("gage config {verb}: {e}");
            std::process::exit(1);
        }
    }
}

/// Flattens a table to `(dotted.name, value)` pairs in key order.
fn leaves(table: &Table) -> Vec<(String, &Value)> {
    let mut out = Vec::new();
    collect_leaves(&mut out, table, "");
    out
}

fn collect_leaves<'a>(out: &mut Vec<(String, &'a Value)>, table: &'a Table, prefix: &str) {
    for (key, value) in table {
        let name = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            Value::Table(inner) => collect_leaves(out, inner, &name),
            leaf => out.push((name, leaf)),
        }
    }
}

fn edit() {
    let target = user_config_path();
    if !target.exists()
        && let Err(e) = Config::default().save_to(&target)
    {
        eprintln!("Error initializing {}: {e}", target.display());
        std::process::exit(1);
    }

    let editor = env::var_os("VISUAL")
        .or_else(|| env::var_os("EDITOR"))
        .unwrap_or_else(|| "vi".into());

    match Command::new(&editor).arg(&target).status() {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("Editor exited with status {s}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Failed to launch editor {}: {e}", editor.to_string_lossy());
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_table() -> Table {
        match Value::try_from(Config::default()).unwrap() {
            Value::Table(t) => t,
            _ => unreachable!(),
        }
    }

    #[test]
    fn leaves_flatten_every_setting_to_a_dotted_name() {
        let names: Vec<String> = leaves(&default_table())
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(
            names,
            [
                "backup.sources",
                "issues.related_threshold",
                "scanners.disable",
                "scanners.enable",
                "storage.max_session_size",
            ]
        );
    }

    #[test]
    fn leaves_nest_dotted_tables() {
        let table: Table = toml::from_str("[source.foo]\nurl = \"claude:~/x\"\n").unwrap();
        let found = leaves(&table);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "source.foo.url");
        assert_eq!(found[0].1.to_string(), "\"claude:~/x\"");
    }
}
