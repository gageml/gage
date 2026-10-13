//! `gage remote`: the Git remotes the store pushes to, fetches from,
//! and backs up to.

use clap::{Args, Subcommand};
use cliclack as cli;
use gage_store::{RemoteStore, Store, StoreError};
use tabled::{
    Table,
    settings::{
        Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::dialog::{self, DialogError};
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

    /// Delete remotes
    ///
    /// Removes the remote and what it tracked. Nothing on the remote
    /// is changed.
    Delete(RemoteDeleteArgs),
}

#[derive(Args)]
pub struct RemoteAddArgs {
    /// Remote name
    name: String,

    /// Remote URL
    url: String,

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
pub struct RemoteDeleteArgs {
    /// Remote names
    #[arg(required = true)]
    names: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub fn add(args: RemoteAddArgs) {
    let store = open_store("gage remote add");
    dialog::run("Add remote", || {
        dialog::step_value("Name", &args.name)?;
        dialog::step_value("URL", &args.url)?;
        if args.backup {
            dialog::step_value("Backup target", "yes")?;
        }
        if !args.yes
            && !cli::confirm(format!("Add remote {}?", args.name))
                .initial_value(true)
                .interact()?
        {
            return Err(DialogError::Canceled);
        }
        let remote = RemoteStore::from(&store)
            .add(&args.name, &args.url, args.backup)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        let backup = if remote.backup {
            " as a backup target"
        } else {
            ""
        };
        Ok(format!("Added remote {} ({}){backup}", remote.name, remote.url).into())
    });
}

pub fn list() {
    let store = open_store("gage remote list");
    let remotes = match RemoteStore::from(&store).list() {
        Ok(remotes) => remotes,
        Err(e) => {
            eprintln!("gage remote list: {e}");
            std::process::exit(1);
        }
    };
    if remotes.is_empty() {
        println!("No remotes found");
        return;
    }
    let header: Vec<String> = ["Name", "URL", "Backup"]
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
}

pub fn delete(args: RemoteDeleteArgs) {
    let store = open_store("gage remote delete");
    let remotes = RemoteStore::from(&store);

    // Resolve every argument before deleting anything, so one bad
    // argument leaves the store untouched
    let known = match remotes.list() {
        Ok(known) => known,
        Err(e) => {
            eprintln!("gage remote delete: {e}");
            std::process::exit(1);
        }
    };
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
        std::process::exit(1);
    }

    let total = args.names.len();
    dialog::run("Delete remotes", || {
        dialog::step_list("Remotes", lines)?;

        if !args.yes {
            let prompt = format!(
                "Delete {}? (nothing on the remote is changed)",
                count(total, "remote")
            );
            let confirmed = cli::confirm(prompt).initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

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
    });
}

/// Open the default store, or print `command: <error>` and exit
fn open_store(command: &str) -> Store {
    match Store::open(&gage_store::store_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    }
}
