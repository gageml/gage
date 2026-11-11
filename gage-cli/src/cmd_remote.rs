//! `gage remote`: the Git remotes the store pushes to, fetches from,
//! and backs up to.

use anyhow::Context;
use clap::{ArgGroup, Args, Subcommand};
use cliclack as cli;
use gage_store::{RemoteStore, Store, StoreError, validate_remote_objects};
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
use crate::style;

#[derive(Subcommand)]
pub enum RemoteCommand {
    /// Add a remote
    ///
    /// Any Git URL or path. A project's own repository is a valid
    /// remote: what Gage stores there is not transferred by a clone or
    /// seen by a checkout.
    Add(RemoteAddArgs),

    /// List remotes
    List,

    /// Edit a remote
    ///
    /// Each option given replaces what the remote recorded for it.
    Edit(RemoteEditArgs),

    /// Delete remotes
    ///
    /// Removes the remote and what it tracked. Nothing on the remote
    /// is changed.
    Delete(RemoteDeleteArgs),
}

/// Paragraph under the short line of every `--push` option
const PUSH_HELP: &str = "gage push sends these objects, and everything they link, when no \
                         object is named. Each one is recorded as given and resolved when \
                         pushed.";

/// Paragraph under the short line of every `--fetch` option
const FETCH_HELP: &str = "gage fetch takes these objects, and everything they link, when no \
                          object is named. Each one is recorded as given and resolved against \
                          what remote has when fetched.";

const PUSH_SHORT: &str = "Object to push (tag, ID, or prefix; repeatable)";
const FETCH_SHORT: &str = "Object to fetch (tag, ID, or prefix; repeatable)";

#[derive(Args)]
pub struct RemoteAddArgs {
    /// Remote name
    name: String,

    /// Remote URL
    url: String,

    #[arg(
        long,
        value_name = "OBJECT",
        help = PUSH_SHORT,
        long_help = format!("{PUSH_SHORT}\n\n{PUSH_HELP}")
    )]
    push: Vec<String>,

    #[arg(
        long,
        value_name = "OBJECT",
        help = FETCH_SHORT,
        long_help = format!("{FETCH_SHORT}\n\n{FETCH_HELP}")
    )]
    fetch: Vec<String>,

    /// Mark remote as a backup target
    ///
    /// `gage backup` sends the whole store to a backup target and
    /// refuses any other remote
    #[arg(long)]
    backup: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
#[command(group = ArgGroup::new("remote_edit_attrs").required(true).multiple(true).args(["push", "fetch"]))]
pub struct RemoteEditArgs {
    /// Remote to edit
    name: String,

    #[arg(
        long,
        value_name = "OBJECT",
        help = PUSH_SHORT,
        long_help = format!("{PUSH_SHORT}\n\n{PUSH_HELP}")
    )]
    push: Vec<String>,

    #[arg(
        long,
        value_name = "OBJECT",
        help = FETCH_SHORT,
        long_help = format!("{FETCH_SHORT}\n\n{FETCH_HELP}")
    )]
    fetch: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct RemoteDeleteArgs {
    /// Remote names
    #[arg(required = true)]
    names: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub fn add(args: RemoteAddArgs) -> Result<(), CliError> {
    let store = open_store("gage remote add")?;
    dialog::run("Add remote", || {
        dialog::step_value("Name", &args.name)?;
        dialog::step_value("URL", &args.url)?;
        if args.backup {
            dialog::step_value("Backup target", "yes")?;
        }
        default_steps(&args.push, &args.fetch)?;
        // Checked before the remote is written, so a bad object leaves
        // no remote behind
        validate_remote_objects(&args.push)
            .and_then(|()| validate_remote_objects(&args.fetch))
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        dialog::confirm(args.yes, true)?;
        let remotes = RemoteStore::from(&store);
        let remote = remotes
            .add(&args.name, &args.url, args.backup)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        set_defaults(&remotes, &args.name, &args.push, &args.fetch)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        let backup = if remote.backup {
            " as a backup target"
        } else {
            ""
        };
        Ok(format!("Added remote {} ({}){backup}", remote.name, remote.url).into())
    })
}

/// Print the default objects of each direction that has any.
fn default_steps(push: &[String], fetch: &[String]) -> std::io::Result<()> {
    if !push.is_empty() {
        dialog::step_list("Push by default", push.iter().cloned())?;
    }
    if !fetch.is_empty() {
        dialog::step_list("Fetch by default", fetch.iter().cloned())?;
    }
    Ok(())
}

/// Record the default objects of each direction that names any. A
/// direction with none is left as the remote has it.
fn set_defaults(
    remotes: &RemoteStore<'_>,
    name: &str,
    push: &[String],
    fetch: &[String],
) -> Result<(), StoreError> {
    if !push.is_empty() {
        remotes.set_push(name, push)?;
    }
    if !fetch.is_empty() {
        remotes.set_fetch(name, fetch)?;
    }
    Ok(())
}

pub fn list() -> Result<(), CliError> {
    let store = open_store("gage remote list")?;
    let remotes = RemoteStore::from(&store)
        .list()
        .context("gage remote list")?;
    if remotes.is_empty() {
        println!("No remotes found");
        return Ok(());
    }
    let header: Vec<String> = ["Name", "URL", "Backup", "Push", "Fetch"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = remotes.iter().map(|r| {
        vec![
            r.name.clone(),
            r.url.clone(),
            if r.backup {
                "yes".to_string()
            } else {
                String::new()
            },
            r.push.join("\n"),
            r.fetch.join("\n"),
        ]
    });
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

pub fn edit(args: RemoteEditArgs) -> Result<(), CliError> {
    let store = open_store("gage remote edit")?;
    let remotes = RemoteStore::from(&store);
    remotes.get(&args.name).context("gage remote edit")?;
    dialog::run("Edit remote", || {
        dialog::step_value("Name", &args.name)?;
        default_steps(&args.push, &args.fetch)?;
        dialog::confirm(args.yes, true)?;
        set_defaults(&remotes, &args.name, &args.push, &args.fetch)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(format!("Updated remote {}", args.name).into())
    })
}

pub fn delete(args: RemoteDeleteArgs) -> Result<(), CliError> {
    let store = open_store("gage remote delete")?;
    let remotes = RemoteStore::from(&store);

    // Resolve every argument before deleting anything, so one bad
    // argument leaves the store untouched
    let known = remotes.list().context("gage remote delete")?;
    let mut lines: Vec<String> = Vec::with_capacity(args.names.len());
    let mut errors = 0;
    for name in &args.names {
        match known.iter().find(|r| &r.name == name) {
            Some(remote) => {
                let backup = if remote.backup { " (backup)" } else { "" };
                lines.push(format!("{} {}{backup}", remote.name, remote.url));
            }
            None => {
                eprintln!(
                    "gage remote delete: {}",
                    StoreError::RemoteNotFound(name.clone())
                );
                errors += 1;
            }
        }
    }
    if errors > 0 {
        return Err(CliError::exit(1));
    }

    let total = args.names.len();
    dialog::run("Delete remotes", || {
        dialog::step_list("Remotes", lines)?;

        cli::log::remark("Nothing on the remote is changed")?;
        dialog::confirm(args.yes, false)?;

        let mut deleted = 0;
        for name in &args.names {
            match remotes.delete(name) {
                Ok(_) => deleted += 1,
                Err(e) => cli::log::warning(format!("{name}: {e}"))?,
            }
        }
        if deleted < total {
            return Err(dialog::partial("Deleted", deleted, total, "remote"));
        }
        Ok(format!("Deleted {}", count(deleted, "remote")).into())
    })
}

/// Open the default store, or `command: <error>`
fn open_store(command: &str) -> Result<Store, CliError> {
    let store = Store::open(&gage_store::store_path()).with_context(|| command.to_string())?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::{Cli, Command};

    use super::*;

    fn remote_command(argv: &[&str]) -> Result<RemoteCommand, clap::Error> {
        let cli = Cli::try_parse_from([&["gage", "remote"], argv].concat())?;
        let Command::Remote { command } = cli.command else {
            panic!("not a remote command");
        };
        Ok(command)
    }

    /// `--push` and `--fetch` repeat on `add` and `edit`, and `edit`
    /// needs at least one of them.
    #[test]
    fn push_and_fetch_repeat_and_edit_needs_one() {
        let Ok(RemoteCommand::Add(add)) = remote_command(&[
            "add",
            "up",
            "/srv/up.git",
            "--push",
            "work",
            "--push",
            "7zzz",
            "--fetch",
            "theirs",
        ]) else {
            panic!("not a remote add");
        };
        assert_eq!(add.push, ["work", "7zzz"]);
        assert_eq!(add.fetch, ["theirs"]);

        let Ok(RemoteCommand::Add(add)) = remote_command(&["add", "up", "/srv/up.git"]) else {
            panic!("not a remote add");
        };
        assert!(add.push.is_empty() && add.fetch.is_empty());

        let Ok(RemoteCommand::Edit(edit)) = remote_command(&["edit", "up", "--fetch", "theirs"])
        else {
            panic!("not a remote edit");
        };
        assert!(edit.push.is_empty());
        assert_eq!(edit.fetch, ["theirs"]);

        let Err(err) = remote_command(&["edit", "up"]) else {
            panic!("edit parsed with no option");
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    /// An edit replaces the default of each direction it names and
    /// leaves the other as the remote has it.
    #[test]
    fn set_defaults_leaves_an_unnamed_direction_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open_with_cache_root(&path, &tmp.path().join("cache")).unwrap();
        let remotes = RemoteStore::from(&store);
        remotes.add("up", "/srv/up.git", false).unwrap();
        let objects = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();

        set_defaults(&remotes, "up", &objects(&["work"]), &objects(&["theirs"])).unwrap();
        set_defaults(&remotes, "up", &objects(&["other"]), &[]).unwrap();
        let remote = remotes.get("up").unwrap();
        assert_eq!(remote.push, ["other"]);
        assert_eq!(remote.fetch, ["theirs"]);
    }
}
