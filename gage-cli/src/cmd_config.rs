use std::env;
use std::process::Command;

use anyhow::Context;
use clap::Subcommand;
use toml::{Table, Value};

use gage_core::config::{Config, user_config_path};
use gage_session::ModelSizes;

use crate::error::CliError;
use crate::source::{driver_models, driver_registry};

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// List effective configuration
    ///
    /// List effective configuration. Every setting is printed as
    /// "name = value" with its working value, which is the value in
    /// config file or the default when the file does not set it.
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
    /// Edit config file in system editor. Opens config file in $VISUAL
    /// or $EDITOR, creating it with default values if absent. Config
    /// file is ~/.gage/config.toml, or $GAGE_HOME/config.toml when
    /// GAGE_HOME is set.
    Edit,
}

pub fn run(command: ConfigCommand) -> Result<(), CliError> {
    match command {
        ConfigCommand::List => list(),
        ConfigCommand::Get { name } => get(&name),
        ConfigCommand::Edit => edit(),
    }
}

fn list() -> Result<(), CliError> {
    let effective = load_effective("list")?;
    for (name, value) in leaves(&effective) {
        println!("{name} = {value}");
    }
    Ok(())
}

fn get(name: &str) -> Result<(), CliError> {
    let effective = load_effective("get")?;
    let Some(value) = leaves(&effective).into_iter().find(|(n, _)| n == name) else {
        return Err(CliError::message(format!(
            "gage config get: no setting named {name}"
        )));
    };
    match value.1 {
        Value::String(s) => println!("{s}"),
        other => println!("{other}"),
    }
    Ok(())
}

/// Loads the user config and converts it to a TOML table so that
/// every setting, set or defaulted, has a dotted name and a value.
fn load_effective(verb: &str) -> Result<Table, CliError> {
    let config = Config::load_user().with_context(|| format!("gage config {verb}"))?;
    let mut table = match Value::try_from(&config).with_context(|| format!("gage config {verb}"))? {
        Value::Table(t) => t,
        _ => unreachable!("Config serializes to a TOML table"),
    };
    let models = driver_models(&config.drivers);
    for driver in driver_registry().drivers() {
        let none = ModelSizes::default();
        let set = models.get(driver.name()).unwrap_or(&none);
        set_driver_model_sizes(
            &mut table,
            driver.name(),
            &driver.model_sizes().overridden_by(set),
        );
    }
    Ok(table)
}

/// Put each size's working model on driver `name` into `table` as
/// `drivers.<name>.models.<size>`. A driver's defaults are its own,
/// so the config alone does not hold them.
fn set_driver_model_sizes(table: &mut Table, name: &str, sizes: &ModelSizes) {
    let models: Table = sizes
        .iter()
        .filter_map(|(size, model)| Some((size.to_string(), Value::String(model?.to_string()))))
        .collect();
    if models.is_empty() {
        return;
    }
    let mut at = table;
    for key in ["drivers", name] {
        let Value::Table(inner) = at.entry(key).or_insert_with(|| Value::Table(Table::new()))
        else {
            unreachable!("drivers and its driver entries serialize as tables");
        };
        at = inner;
    }
    at.insert("models".to_string(), Value::Table(models));
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

fn edit() -> Result<(), CliError> {
    let target = user_config_path();
    if !target.exists() {
        Config::default()
            .save_to(&target)
            .with_context(|| format!("Error initializing {}", target.display()))?;
    }

    let editor = env::var_os("VISUAL")
        .or_else(|| env::var_os("EDITOR"))
        .unwrap_or_else(|| "vi".into());

    let status = Command::new(&editor)
        .arg(&target)
        .status()
        .with_context(|| format!("Failed to launch editor {}", editor.to_string_lossy()))?;
    if !status.success() {
        return Err(CliError::message(format!(
            "Editor exited with status {status}"
        )));
    }
    Ok(())
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
                "backup.native",
                "drivers.default",
                "issues.related_threshold",
                "scanners.disable",
                "scanners.enable",
                "storage.max_session_size",
            ]
        );
    }

    #[test]
    fn a_drivers_model_sizes_are_listed_under_its_name() {
        let mut table = default_table();
        let sizes = ModelSizes {
            small: Some("s".to_string()),
            medium: None,
            large: Some("l".to_string()),
        };
        set_driver_model_sizes(&mut table, "fake", &sizes);
        set_driver_model_sizes(&mut table, "sizeless", &ModelSizes::default());
        let listed: Vec<(String, String)> = leaves(&table)
            .into_iter()
            .filter(|(name, _)| name.starts_with("drivers."))
            .map(|(name, value)| (name, value.to_string()))
            .collect();
        assert_eq!(
            listed,
            [
                ("drivers.default".to_string(), "\"claude\"".to_string()),
                ("drivers.fake.models.large".to_string(), "\"l\"".to_string()),
                ("drivers.fake.models.small".to_string(), "\"s\"".to_string()),
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
