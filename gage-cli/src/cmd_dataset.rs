use std::sync::{Arc, Mutex};

use clap::{Args, Subcommand};
use datafusion::arrow::array::{Int64Array, StringArray, TimestampMillisecondArray};
use gage_core::uuid::short_uuid;
use gage_query2::ContextBuilder;
use gage_store::{DatasetStore, Store};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::cmd_note::count_rows;
use crate::cmd_session::{column, run_query};
use crate::human::format_elapsed_ms;
use crate::style::{self, IdKind, styled_id};

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
    println!("Created dataset {}", short_uuid(&id));
}

pub async fn list(args: DatasetListArgs) {
    let store = open_store("gage dataset list");
    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let total = count_rows(&ctx, "SELECT COUNT(*) FROM dataset").await;
    if total == 0 {
        println!("No datasets found");
        return;
    }
    let show = args.limit.show_count(total);
    let sql = format!(
        "SELECT d.id, d.id_prefix, d.created, COUNT(m.session_id) \
         FROM dataset d LEFT JOIN dataset_session m ON m.dataset_id = d.id \
         GROUP BY d.id, d.id_prefix, d.created, d.modified \
         ORDER BY d.modified DESC LIMIT {show}"
    );
    let batches = run_query(&ctx, &sql).await;

    let header: Vec<String> = ["Id", "Sessions", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in &batches {
        let ids = column::<StringArray>(batch, 0);
        let prefixes = column::<StringArray>(batch, 1);
        let createds = column::<TimestampMillisecondArray>(batch, 2);
        let counts = column::<Int64Array>(batch, 3);
        for i in 0..batch.num_rows() {
            rows.push(vec![
                styled_id(short_uuid(ids.value(i)), prefixes.value(i), IdKind::Gage),
                counts.value(i).to_string(),
                format_elapsed_ms(createds.value(i)),
            ]);
        }
    }
    let shown = rows.len();
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::one(1), Alignment::right())
        .modify(Columns::new(1..).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    args.limit.print_summary(shown, total, "dataset");
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
