use std::env;
use std::fs;
use std::io;
use std::process::Command;

use clap::Subcommand;
use console::style;
use toml::{Table, Value};

use gage_core::config::{Config, display_user_config_path, user_config_path};

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Show effective configuration
    ///
    /// Show effective configuration. Every setting is listed with its
    /// effective value; settings not present in the config file are
    /// marked as defaults.
    Show,

    /// Edit config file in system editor
    ///
    /// Edit config file in system editor. Opens ~/.gage/config.toml in
    /// $VISUAL or $EDITOR, creating it with default values if absent.
    Edit,
}

pub fn run(command: ConfigCommand) {
    match command {
        ConfigCommand::Show => show(),
        ConfigCommand::Edit => edit(),
    }
}

fn show() {
    let config = match Config::load_user() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gage config show: {e}");
            std::process::exit(1);
        }
    };
    let effective = match Value::try_from(&config) {
        Ok(Value::Table(t)) => t,
        Ok(_) => unreachable!("Config serializes to a TOML table"),
        Err(e) => {
            eprintln!("gage config show: {e}");
            std::process::exit(1);
        }
    };
    let set = match read_set_table() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("gage config show: {e}");
            std::process::exit(1);
        }
    };
    let marker = style("# default").dim().to_string();
    println!("# {}", display_user_config_path());
    print!("{}", render(&effective, &set, &marker));
}

/// Reads the config file as a raw TOML table, so that `render` can
/// tell which settings the file supplies. An absent file is an empty
/// table.
fn read_set_table() -> io::Result<Table> {
    match fs::read_to_string(user_config_path()) {
        Ok(s) => toml::from_str(&s).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Table::new()),
        Err(e) => Err(e),
    }
}

/// Renders `effective` as TOML. A leaf whose dotted path is absent from
/// `set` carries `marker` after its value. Tables render as `[dotted]`
/// sections, each preceded by a blank line.
fn render(effective: &Table, set: &Table, marker: &str) -> String {
    let mut out = String::new();
    render_table(&mut out, effective, set, &[], marker);
    out
}

fn render_table(out: &mut String, table: &Table, set: &Table, path: &[&str], marker: &str) {
    let leaves = table.iter().filter(|(_, v)| !v.is_table());
    let mut wrote_leaf = false;
    for (key, value) in leaves {
        if !wrote_leaf && !path.is_empty() {
            out.push_str(&format!("\n[{}]\n", path.join(".")));
        }
        wrote_leaf = true;
        let mut full: Vec<&str> = path.to_vec();
        full.push(key);
        out.push_str(&format!("{key} = {value}"));
        if !is_set(set, &full) {
            out.push_str(&format!("  {marker}"));
        }
        out.push('\n');
    }
    for (key, value) in table {
        if let Value::Table(inner) = value {
            let mut full: Vec<&str> = path.to_vec();
            full.push(key);
            render_table(out, inner, set, &full, marker);
        }
    }
}

fn is_set(set: &Table, path: &[&str]) -> bool {
    let mut cur = set;
    let (last, parents) = path.split_last().expect("render passes a non-empty path");
    for key in parents {
        match cur.get(*key) {
            Some(Value::Table(t)) => cur = t,
            _ => return false,
        }
    }
    cur.contains_key(*last)
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

    fn tables(set: &str) -> (Table, Table) {
        let effective = match Value::try_from(Config::default()).unwrap() {
            Value::Table(t) => t,
            _ => unreachable!(),
        };
        (effective, toml::from_str(set).unwrap())
    }

    #[test]
    fn render_marks_unset_leaves_and_leaves_set_ones_plain() {
        let (effective, set) = tables("[issues]\nrelated_threshold = 0.2\n");
        let out = render(&effective, &set, "# default");
        assert!(
            out.contains("\n[issues]\nrelated_threshold = 0.2\n"),
            "{out}"
        );
        assert!(
            out.contains("max_session_size = \"256MB\"  # default\n"),
            "{out}"
        );
        assert!(out.contains("enable = []  # default\n"), "{out}");
    }

    #[test]
    fn render_lists_every_section() {
        let (effective, set) = tables("");
        let out = render(&effective, &set, "#");
        for section in ["[issues]", "[scanners]", "[storage]"] {
            assert!(out.contains(section), "{out}");
        }
    }

    #[test]
    fn render_nests_dotted_sections() {
        let mut effective = Table::new();
        let mut foo = Table::new();
        foo.insert("url".into(), Value::String("claude:~/x".into()));
        let mut source = Table::new();
        source.insert("foo".into(), Value::Table(foo));
        effective.insert("source".into(), Value::Table(source));
        let set: Table = toml::from_str("[source.foo]\nurl = \"claude:~/x\"\n").unwrap();
        assert_eq!(
            render(&effective, &set, "#"),
            "\n[source.foo]\nurl = \"claude:~/x\"\n"
        );
    }
}
