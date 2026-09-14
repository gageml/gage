use clap::Subcommand;
use gage_claude::project::shorten_home_path;
use gage_store::{InitOutcome, StoreStatus};
use tabled::{
    Table,
    settings::{Style, object::Columns},
};

use crate::human::format_size;
use crate::style;

#[derive(Subcommand)]
pub enum StoreCommand {
    /// Create the Gage store
    ///
    /// Creates a bare Git repository at `$GAGE_HOME/store.git`. Running
    /// it on an existing store is harmless.
    Init,

    /// Show store status
    Status,
}

pub fn run(command: StoreCommand) {
    match command {
        StoreCommand::Init => init(),
        StoreCommand::Status => status(),
    }
}

fn init() {
    let outcome = match gage_store::init() {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("gage store init: {e}");
            std::process::exit(1);
        }
    };
    let verb = match outcome {
        InitOutcome::Created => "Initialized empty",
        InitOutcome::Reinitialized => "Reinitialized existing",
    };
    println!(
        "{verb} Gage store in {}/",
        gage_store::store_path().display()
    );
}

fn status() {
    let status = match gage_store::status() {
        Ok(status) => status,
        Err(e) => {
            eprintln!("gage store status: {e}");
            std::process::exit(1);
        }
    };
    let rows = status_rows(&status);
    let table = Table::from_iter(rows)
        .with(Style::rounded().horizontals([]))
        .modify(Columns::first(), style::dim())
        .to_string();
    println!("{table}");
}

fn status_rows(status: &StoreStatus) -> Vec<Vec<String>> {
    let remotes = status
        .remotes
        .iter()
        .map(|r| format!("{} {}", r.name, r.url))
        .collect::<Vec<_>>()
        .join("\n");
    [
        ("path", shorten_home_path(&status.path)),
        (
            "objects",
            (status.loose_objects + status.packed_objects).to_string(),
        ),
        ("loose", status.loose_objects.to_string()),
        ("packs", status.packs.to_string()),
        ("size", format_size(status.size as i64)),
        ("refs", status.refs.to_string()),
        ("remotes", remotes),
    ]
    .into_iter()
    .map(|(k, v)| vec![k.to_string(), v])
    .collect()
}
