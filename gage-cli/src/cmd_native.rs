//! `gage native`: the names the config files give to native session
//! sources.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Args, Subcommand};
use cliclack as cli;
use gage_core::config::{
    Config, DEFAULT_NATIVE, gage_home, project_gage_dirs, remove_native_source,
    rename_backup_native, rename_native_source, set_native_source, user_config_path,
};
use gage_core::path::shorten_home;
use gage_registry::driver::DriverRegistry;
use tabled::{
    Table,
    settings::{
        Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::dialog::{self, DialogError};
use crate::error::CliError;
use crate::human::count;
use crate::source::{check_native_entry, driver_registry, project_native};
use crate::style;

#[derive(Subcommand)]
pub enum NativeCommand {
    /// Add a native source
    ///
    /// Gives a name to a session source. The name is then accepted
    /// wherever a session source is, and the name "default" is the
    /// source a command reads when it is given none.
    Add(NativeAddArgs),

    /// List native sources
    ///
    /// Lists each name in effect in the current directory with its
    /// source and the config file that sets it. A scheme that drivers
    /// define is listed as built in.
    List,

    /// Edit a native source
    ///
    /// Replaces the source of a name in the config file that sets it.
    Edit(NativeEditArgs),

    /// Rename a native source
    ///
    /// The source keeps its value and its config file. A backup
    /// setting that lists the name is updated.
    Rename(NativeRenameArgs),

    /// Delete native sources
    ///
    /// Removes each name from the config file that sets it. No
    /// session is deleted.
    Delete(NativeDeleteArgs),
}

/// Paragraph under the short line of every source argument
const SOURCE_HELP: &str = "Session directory as SCHEME:PATH, or SCHEME alone for the default \
                           directory of that scheme. Schemes are claude, codex, and pi. A \
                           relative PATH is accepted in a project config file and is under \
                           the project directory.";

const SOURCE_SHORT: &str = "Session source";

#[derive(Args)]
pub struct NativeAddArgs {
    /// Native source name
    name: String,

    #[arg(help = SOURCE_SHORT, long_help = format!("{SOURCE_SHORT}\n\n{SOURCE_HELP}"))]
    source: String,

    /// Write project config file
    ///
    /// The file is .gage/config.toml under the nearest directory, from
    /// the current one up, that has a .gage directory, and under the
    /// current directory when none has. The name is in effect in that
    /// directory and below. Without this option the name is written
    /// to the user config file.
    #[arg(long)]
    project: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct NativeEditArgs {
    /// Native source to edit
    name: String,

    #[arg(help = SOURCE_SHORT, long_help = format!("{SOURCE_SHORT}\n\n{SOURCE_HELP}"))]
    source: String,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct NativeRenameArgs {
    /// Native source to rename
    name: String,

    /// New native source name
    new_name: String,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct NativeDeleteArgs {
    /// Native source names
    #[arg(required = true)]
    names: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub fn add(args: NativeAddArgs) -> Result<(), CliError> {
    let layers = Layers::load().context("gage native add")?;
    let file = if args.project {
        layers.project_file()
    } else {
        layers.user_file().clone()
    };
    dialog::run("Add native source", || {
        dialog::step_value("Name", &args.name)?;
        dialog::step_value("Source", &args.source)?;
        dialog::step_value("Config file", file.shown())?;
        // Checked before the file is written, so a bad name or source
        // leaves it as it was
        file.check_new(&args.name, &args.source).map_err(failed)?;
        if let Some(inner) = layers.replaced_in(&file, &args.name) {
            cli::log::remark(format!(
                "{} sets {} in this directory and replaces this source",
                inner.shown(),
                args.name
            ))?;
        }
        dialog::confirm(args.yes, true)?;
        set_native_source(&file.path, &args.name, &args.source).map_err(failed)?;
        Ok(format!("Added native source {} ({})", args.name, args.source).into())
    })
}

pub fn list() -> Result<(), CliError> {
    let layers = Layers::load().context("gage native list")?;
    let header = ["Name", "Source", "Set in"].map(String::from);
    let rows = layers.rows(&driver_registry());
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(
            Columns::one(0).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_CYAN),
        )
        .to_string();
    println!("{table}");
    Ok(())
}

pub fn edit(args: NativeEditArgs) -> Result<(), CliError> {
    let layers = Layers::load().context("gage native edit")?;
    let file = layers.owner(&args.name).context("gage native edit")?.file;
    dialog::run("Edit native source", || {
        dialog::step_value("Name", &args.name)?;
        dialog::step_value("Source", &args.source)?;
        dialog::step_value("Config file", file.shown())?;
        check_native_entry(&args.name, &args.source, file.project_dir()).map_err(failed)?;
        dialog::confirm(args.yes, true)?;
        set_native_source(&file.path, &args.name, &args.source).map_err(failed)?;
        Ok(format!("Updated native source {}", args.name).into())
    })
}

pub fn rename(args: NativeRenameArgs) -> Result<(), CliError> {
    let layers = Layers::load().context("gage native rename")?;
    let Set { file, source } = layers.owner(&args.name).context("gage native rename")?;
    dialog::run("Rename native source", || {
        dialog::step_value("Name", &args.name)?;
        dialog::step_value("New name", &args.new_name)?;
        dialog::step_value("Config file", file.shown())?;
        file.check_new(&args.new_name, source).map_err(failed)?;
        let in_backup = layers.backup_needs(file, &args.name);
        if in_backup {
            cli::log::remark(format!(
                "backup.native lists {} and is updated in {}",
                args.name,
                layers.user_file().shown()
            ))?;
        }
        dialog::confirm(args.yes, true)?;
        rename_native_source(&file.path, &args.name, &args.new_name).map_err(failed)?;
        if in_backup {
            rename_backup_native(&layers.user_file().path, &args.name, &args.new_name)
                .map_err(failed)?;
        }
        Ok(format!("Renamed native source {} to {}", args.name, args.new_name).into())
    })
}

pub fn delete(args: NativeDeleteArgs) -> Result<(), CliError> {
    let layers = Layers::load().context("gage native delete")?;

    // Resolve every argument before deleting anything, so one bad
    // argument leaves the config files untouched
    let mut found: Vec<(&str, Set<'_>)> = Vec::with_capacity(args.names.len());
    let mut errors = 0;
    for name in &args.names {
        match layers.deletable(name) {
            Ok(set) => found.push((name.as_str(), set)),
            Err(e) => {
                eprintln!("gage native delete: {e:#}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        return Err(CliError::exit(1));
    }

    let total = found.len();
    dialog::run("Delete native sources", || {
        dialog::step_list(
            "Native sources",
            found
                .iter()
                .map(|(name, set)| format!("{name} {} ({})", set.source, set.file.shown())),
        )?;

        cli::log::remark("No session is deleted")?;
        dialog::confirm(args.yes, false)?;

        let mut deleted = 0;
        for (name, set) in &found {
            match remove_native_source(&set.file.path, name) {
                Ok(true) => deleted += 1,
                Ok(false) => cli::log::warning(format!("{name}: no longer set"))?,
                Err(e) => cli::log::warning(format!("{name}: {e}"))?,
            }
        }
        if deleted < total {
            return Err(dialog::partial("Deleted", deleted, total, "native source"));
        }
        Ok(format!("Deleted {}", count(deleted, "native source")).into())
    })
}

/// The failure of a check or a write, shown as the dialog's outcome
fn failed(e: impl Into<anyhow::Error>) -> DialogError {
    DialogError::Failed(format!("{:#}", e.into()))
}

/// The value of the `Set in` column for a scheme that drivers define
const BUILT_IN: &str = "built in";

/// The value of the `Set in` column for the default source when no
/// config file sets one: the default directory of the default driver
const DEFAULT_DRIVER_SETTING: &str = "drivers.default";

/// One config file that sets native sources
#[derive(Clone, Debug, PartialEq)]
struct ConfigFile {
    path: PathBuf,
    /// Whether the file is a project config and not the user config
    project: bool,
    /// Each name the file sets with its source. A relative path of a
    /// project config is already placed under the project directory.
    sources: BTreeMap<String, String>,
}

impl ConfigFile {
    fn shown(&self) -> String {
        shorten_home(&self.path)
    }

    /// The directory that places a relative path of this file: the
    /// project directory, which holds `.gage`. The user config has
    /// none.
    fn project_dir(&self) -> Option<&Path> {
        self.project.then(|| {
            self.path
                .parent()
                .and_then(Path::parent)
                .expect("a project config should be at <project>/.gage/config.toml")
        })
    }

    /// Check that `name` can be set to `source` in this file as a new
    /// entry: the file does not set the name, and the entry is one a
    /// config load accepts.
    fn check_new(&self, name: &str, source: &str) -> anyhow::Result<()> {
        if self.sources.contains_key(name) {
            anyhow::bail!("{name} is already set in {}", self.shown());
        }
        check_native_entry(name, source, self.project_dir())
    }
}

/// A name as a config file sets it
#[derive(Clone, Copy, Debug)]
struct Set<'a> {
    file: &'a ConfigFile,
    source: &'a str,
}

/// The config files that set native sources for the current
/// directory, and the settings of the user config that refer to them
struct Layers {
    /// Each file, innermost project first. The user config is last
    /// and is present whether or not its file exists.
    files: Vec<ConfigFile>,
    /// The `backup.native` list of the user config
    backup: Vec<String>,
    /// The `drivers.default` setting of the user config
    default_driver: String,
    /// The project config `gage native add --project` writes
    project_path: PathBuf,
}

impl Layers {
    fn load() -> anyhow::Result<Self> {
        let user_path = user_config_path();
        let config = Config::load_from(&user_path)
            .with_context(|| format!("reading {}", shorten_home(&user_path)))?;
        let cwd = env::current_dir().context("reading the current directory")?;
        let mut files: Vec<ConfigFile> = project_native()?
            .into_iter()
            .rev()
            .map(|project| ConfigFile {
                path: project.path,
                project: true,
                sources: project.sources,
            })
            .collect();
        files.push(ConfigFile {
            path: user_path,
            project: false,
            sources: config.native,
        });
        Ok(Self {
            files,
            backup: config.backup.native,
            default_driver: config.drivers.default,
            project_path: project_config_target(&cwd),
        })
    }

    fn user_file(&self) -> &ConfigFile {
        self.files.last().expect("the user config should be last")
    }

    /// The project config `--project` writes, with the sources it
    /// sets now. The file may not exist yet.
    fn project_file(&self) -> ConfigFile {
        self.files
            .iter()
            .find(|file| file.project && file.path == self.project_path)
            .cloned()
            .unwrap_or_else(|| ConfigFile {
                path: self.project_path.clone(),
                project: true,
                sources: BTreeMap::new(),
            })
    }

    /// The file whose source `name` stands for in the current
    /// directory. A name no file sets is the error.
    fn owner(&self, name: &str) -> anyhow::Result<Set<'_>> {
        self.setter(name)
            .ok_or_else(|| anyhow::anyhow!("{name} is not set in a config file"))
    }

    /// The innermost file that sets `name`, when one does
    fn setter(&self, name: &str) -> Option<Set<'_>> {
        self.files.iter().find_map(|file| {
            let source = file.sources.get(name)?;
            Some(Set { file, source })
        })
    }

    /// The file that sets `name` in place of `file` in the current
    /// directory, when one does
    fn replaced_in(&self, file: &ConfigFile, name: &str) -> Option<&ConfigFile> {
        self.files
            .iter()
            .take_while(|inner| inner.path != file.path)
            .find(|inner| inner.sources.contains_key(name))
    }

    /// Whether `backup.native` lists `name` and would list a name that
    /// stands for nothing once `file` no longer sets it. Another file
    /// that sets the name, or a scheme of the same name, keeps the
    /// list entry a source.
    fn backup_needs(&self, file: &ConfigFile, name: &str) -> bool {
        let elsewhere = self
            .files
            .iter()
            .any(|other| other.path != file.path && other.sources.contains_key(name));
        self.backup.iter().any(|listed| listed == name)
            && !elsewhere
            && driver_registry().for_scheme(name).is_none()
    }

    /// The file `gage native delete` removes `name` from. A name no
    /// file sets, and a name `backup.native` needs, are errors.
    fn deletable(&self, name: &str) -> anyhow::Result<Set<'_>> {
        let set = self.owner(name)?;
        if self.backup_needs(set.file, name) {
            anyhow::bail!(
                "{name} is listed in backup.native of {}; remove it from that list first",
                self.user_file().shown()
            );
        }
        Ok(set)
    }

    /// The rows of `gage native list`, each a name, its source, and
    /// where it is set: the default source, then each scheme of
    /// `registry`, then every other name in order.
    fn rows(&self, registry: &DriverRegistry) -> Vec<[String; 3]> {
        let mut schemes: Vec<&str> = Vec::new();
        for scheme in registry.drivers().iter().flat_map(|d| d.schemes()) {
            if !schemes.contains(scheme) {
                schemes.push(scheme);
            }
        }
        let default_scheme = registry
            .for_name(&self.default_driver)
            .and_then(|driver| driver.schemes().first().copied())
            .unwrap_or(self.default_driver.as_str());

        let mut rows = vec![self.row(
            DEFAULT_NATIVE,
            &format!("{default_scheme}:"),
            DEFAULT_DRIVER_SETTING,
        )];
        for scheme in &schemes {
            rows.push(self.row(scheme, &format!("{scheme}:"), BUILT_IN));
        }
        let names: BTreeSet<&str> = self
            .files
            .iter()
            .flat_map(|file| file.sources.keys().map(String::as_str))
            .filter(|name| *name != DEFAULT_NATIVE && !schemes.contains(name))
            .collect();
        for name in names {
            rows.push(self.row(name, "", ""));
        }
        rows
    }

    /// The row of `name`: its source and file when a file sets it,
    /// and `source` and `set_in` when none does
    fn row(&self, name: &str, source: &str, set_in: &str) -> [String; 3] {
        match self.setter(name) {
            Some(set) => [name.to_string(), set.source.to_string(), set.file.shown()],
            None => [name.to_string(), source.to_string(), set_in.to_string()],
        }
    }
}

/// The project config under the nearest directory, from `start` up,
/// that has a `.gage` directory, and under `start` when none has. The
/// gage home is the user's and is never a project.
fn project_config_target(start: &Path) -> PathBuf {
    let user = gage_home();
    project_gage_dirs(start)
        .into_iter()
        .find(|dir| *dir != user)
        .unwrap_or_else(|| start.join(".gage"))
        .join("config.toml")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::{Cli, Command};

    use super::*;

    fn native_command(argv: &[&str]) -> Result<NativeCommand, clap::Error> {
        let cli = Cli::try_parse_from([&["gage", "native"], argv].concat())?;
        let Command::Native { command } = cli.command else {
            panic!("not a native command");
        };
        Ok(command)
    }

    #[test]
    fn each_subcommand_parses_its_arguments() {
        let Ok(NativeCommand::Add(add)) =
            native_command(&["add", "work", "pi:~/w", "--project", "-y"])
        else {
            panic!("not a native add");
        };
        assert_eq!((add.name.as_str(), add.source.as_str()), ("work", "pi:~/w"));
        assert!(add.project && add.yes);

        let Ok(NativeCommand::Edit(edit)) = native_command(&["edit", "work", "codex"]) else {
            panic!("not a native edit");
        };
        assert_eq!(
            (edit.name.as_str(), edit.source.as_str()),
            ("work", "codex")
        );
        assert!(!edit.yes);

        let Ok(NativeCommand::Rename(rename)) = native_command(&["rename", "old", "new"]) else {
            panic!("not a native rename");
        };
        assert_eq!(
            (rename.name.as_str(), rename.new_name.as_str()),
            ("old", "new")
        );

        let Ok(NativeCommand::Delete(delete)) = native_command(&["delete", "a", "b", "--yes"])
        else {
            panic!("not a native delete");
        };
        assert_eq!(delete.names, ["a", "b"]);
        assert!(delete.yes);

        assert!(matches!(native_command(&["list"]), Ok(NativeCommand::List)));
        for missing in [&["add", "work"][..], &["edit", "work"], &["delete"]] {
            let Err(err) = native_command(missing) else {
                panic!("parsed {missing:?}");
            };
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{missing:?}"
            );
        }
    }

    fn file(path: &str, project: bool, entries: &[(&str, &str)]) -> ConfigFile {
        ConfigFile {
            path: PathBuf::from(path),
            project,
            sources: entries
                .iter()
                .map(|(name, source)| (name.to_string(), source.to_string()))
                .collect(),
        }
    }

    /// A project at `/p` under a user config, with `backup` as the
    /// user's `backup.native`
    fn layers(project: &[(&str, &str)], user: &[(&str, &str)], backup: &[&str]) -> Layers {
        Layers {
            files: vec![
                file("/p/.gage/config.toml", true, project),
                file("/u/config.toml", false, user),
            ],
            backup: backup.iter().map(|name| name.to_string()).collect(),
            default_driver: "claude".to_string(),
            project_path: PathBuf::from("/p/.gage/config.toml"),
        }
    }

    #[test]
    fn a_name_belongs_to_the_innermost_file_that_sets_it() {
        let layers = layers(
            &[("work", "pi:/p/w")],
            &[("work", "codex"), ("old", "pi")],
            &[],
        );
        assert_eq!(
            layers.owner("work").unwrap().file.path,
            layers.files[0].path
        );
        assert_eq!(layers.owner("old").unwrap().file.path, layers.files[1].path);
        assert_eq!(
            layers.owner("none").unwrap_err().to_string(),
            "none is not set in a config file"
        );
        // A scheme no file sets is built in and has no file
        assert!(layers.owner("claude").is_err());

        let replaced = layers.replaced_in(layers.user_file(), "work").unwrap();
        assert_eq!(replaced.path, layers.files[0].path);
        assert!(layers.replaced_in(layers.user_file(), "old").is_none());
        assert!(layers.replaced_in(&layers.files[0], "work").is_none());
    }

    #[test]
    fn a_new_entry_is_checked_against_its_file_and_as_a_config_load_checks_it() {
        let layers = layers(&[("work", "pi:/p/w")], &[("old", "pi")], &[]);
        let (project, user) = (&layers.files[0], &layers.files[1]);
        let refused = |file: &ConfigFile, name: &str, source: &str| {
            format!("{:#}", file.check_new(name, source).unwrap_err())
        };

        assert!(user.check_new("work", "codex:~/w").is_ok());
        // A scheme name replaces the scheme written alone
        assert!(user.check_new("claude", "claude:/x").is_ok());
        assert!(project.check_new("rel", "pi:sessions").is_ok());
        assert_eq!(
            refused(project, "work", "codex"),
            "work is already set in /p/.gage/config.toml"
        );
        assert_eq!(
            refused(user, "rel", "pi:sessions"),
            "native.rel: \"pi:sessions\" has a relative path; write an absolute path or a path \
             under ~"
        );
        assert_eq!(
            refused(user, "other", "old"),
            "native.other: \"old\" is not a source; write it as SCHEME:PATH or SCHEME, where \
             SCHEME is one of claude, codex, pi"
        );
        assert_eq!(
            refused(user, "a:b", "pi"),
            "native.\"a:b\": a name must not contain a colon"
        );
    }

    #[test]
    fn a_name_the_backup_list_needs_is_not_deleted() {
        let layers = layers(
            &[("both", "pi:/p/b")],
            &[
                ("work", "pi:~/w"),
                ("both", "codex"),
                ("claude", "claude:/x"),
            ],
            &["work", "both", "claude", "pi:"],
        );
        let user = layers.user_file();
        assert!(layers.backup_needs(user, "work"));
        // The project still sets the name
        assert!(!layers.backup_needs(user, "both"));
        // The scheme of the same name is still a source
        assert!(!layers.backup_needs(user, "claude"));

        assert_eq!(
            layers.deletable("work").unwrap_err().to_string(),
            "work is listed in backup.native of /u/config.toml; remove it from that list first"
        );
        assert_eq!(
            layers.deletable("both").unwrap().file.path,
            layers.files[0].path
        );
        assert_eq!(layers.deletable("claude").unwrap().file.path, user.path);
        assert!(layers.deletable("none").is_err());
    }

    #[test]
    fn rows_are_the_default_then_the_schemes_then_the_names() {
        let row = |cells: [&str; 3]| cells.map(String::from);
        let registry = driver_registry();

        let unset = layers(&[], &[], &[]);
        assert_eq!(
            unset.rows(&registry),
            [
                row(["default", "claude:", "drivers.default"]),
                row(["claude", "claude:", "built in"]),
                row(["codex", "codex:", "built in"]),
                row(["pi", "pi:", "built in"]),
            ]
        );

        let mut set = layers(
            &[("default", "pi:/p/s"), ("work", "pi:/p/w")],
            &[("work", "codex"), ("codex", "codex:/c"), ("a", "claude")],
            &[],
        );
        set.default_driver = "pi".to_string();
        assert_eq!(
            set.rows(&registry),
            [
                row(["default", "pi:/p/s", "/p/.gage/config.toml"]),
                row(["claude", "claude:", "built in"]),
                row(["codex", "codex:/c", "/u/config.toml"]),
                row(["pi", "pi:", "built in"]),
                row(["a", "claude", "/u/config.toml"]),
                row(["work", "pi:/p/w", "/p/.gage/config.toml"]),
            ]
        );
        set.files[0].sources.remove("default");
        assert_eq!(
            set.rows(&registry)[0],
            row(["default", "pi:", "drivers.default"])
        );
    }

    #[test]
    fn a_project_source_is_written_under_the_nearest_gage_directory() {
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("outer/inner");
        std::fs::create_dir_all(&inner).unwrap();
        assert_eq!(
            project_config_target(&inner),
            inner.join(".gage/config.toml")
        );
        std::fs::create_dir_all(root.path().join("outer/.gage")).unwrap();
        assert_eq!(
            project_config_target(&inner),
            root.path().join("outer/.gage/config.toml")
        );
    }
}
