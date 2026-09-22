use clap::{Args, Subcommand};
use gage_store::{DATASET_TYPE, DatasetRecord, DatasetStore, Store};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::human::format_elapsed_ms;
use crate::style::{self, IdHighlighter};

#[derive(Subcommand)]
pub enum DatasetCommand {
    /// Add an empty dataset
    Add,

    /// List datasets
    List(DatasetListArgs),
}

#[derive(Args)]
pub struct DatasetListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,
}

pub fn add() {
    let store = open_store("gage dataset add");
    let id = match DatasetStore::from(&store).create() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage dataset add: {e}");
            std::process::exit(1);
        }
    };
    println!("Created dataset {id}");
}

pub fn list(args: DatasetListArgs) {
    let store = open_store("gage dataset list");
    let datasets = DatasetStore::from(&store);
    let total = match datasets.query().count() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("gage dataset list: {e}");
            std::process::exit(1);
        }
    };
    if total == 0 {
        println!("No datasets found");
        return;
    }
    let show = args.limit.show_count(total);
    let records: Vec<DatasetRecord> = match datasets
        .query()
        .limit(show)
        .iter()
        .and_then(|it| it.collect())
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gage dataset list: {e}");
            std::process::exit(1);
        }
    };

    // The highlighted prefix is unique within the short-prefix set
    // of datasets, where a dataset prefix resolves first
    let peers = match store.short_prefix_ids(Some(DATASET_TYPE)) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("gage dataset list: {e}");
            std::process::exit(1);
        }
    };
    let highlighter = IdHighlighter::new(peers);

    let header: Vec<String> = ["Id", "Sessions", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = records.iter().map(|r| {
        vec![
            highlighter.short(&r.id),
            r.session_count.to_string(),
            format_elapsed_ms(r.created_ms),
        ]
    });
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::one(1), Alignment::right())
        .modify(Columns::new(1..).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    args.limit.print_summary(records.len(), total, "dataset");
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
