use std::sync::{Arc, Mutex};

use clap::{Args, Subcommand};
use datafusion::arrow::array::{Int64Array, StringArray, TimestampMillisecondArray};
use gage_core::config::{ByteSize, Config};
use gage_core::uuid::short_uuid;
use gage_query2::ContextBuilder;
use gage_store::{DatasetStore, SessionSpec, Store};
use tabled::{
    Table,
    settings::{
        Alignment, Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::cmd_note::count_rows;
use crate::cmd_session::{column, parse_byte_size, print_add_outcome, run_query};
use crate::human::format_elapsed_ms;
use crate::session_select::SessionSelectArgs;
use crate::source;
use crate::style::{self, IdKind, styled_id};

#[derive(Subcommand)]
pub enum DatasetCommand {
    /// Add a dataset
    ///
    /// Without session-selection options, adds an empty dataset. With
    /// options, adds a dataset populated with the selected native
    /// sessions in a single step.
    Add(DatasetAddArgs),

    /// List datasets
    List(DatasetListArgs),
}

#[derive(Args)]
pub struct DatasetAddArgs {
    /// Session source
    ///
    /// A driver scheme selects the driver (`claude:<path>`); a value
    /// with no scheme goes to the default driver (`<path>`). Defaults
    /// to the default driver's default location.
    #[arg(short, long, value_name = "SOURCE", display_order = 1)]
    pub source: Option<String>,

    /// Maximum stored size per session
    ///
    /// Overrides the configured default. Accepts a byte count or a
    /// unit suffix, such as 256MB or 1GB. A session whose stored files
    /// exceed this is refused.
    #[arg(long, value_name = "SIZE", value_parser = parse_byte_size, display_order = 4)]
    pub max_size: Option<ByteSize>,

    /// Add regardless of size
    ///
    /// Stores the session even when it exceeds the size limit.
    #[arg(long, display_order = 5)]
    pub force: bool,

    #[command(flatten)]
    pub select: SessionSelectArgs,
}

#[derive(Args)]
pub struct DatasetListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,
}

pub async fn add(args: DatasetAddArgs) {
    let store = open_store("gage dataset add");
    let dataset_id = match DatasetStore::from(&store).create() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("gage dataset add: {e}");
            std::process::exit(1);
        }
    };
    println!("Created dataset {}", short_uuid(&dataset_id));

    if args.select.is_empty() {
        return;
    }

    let selected = match args.select.resolve("gage dataset add").await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("gage dataset add: {e}");
            std::process::exit(1);
        }
    };
    if selected.is_empty() {
        eprintln!("gage dataset add: no sessions matched the selection");
        return;
    }

    // --force lifts the cap; otherwise the flag overrides the
    // configured default.
    let max_bytes = if args.force {
        None
    } else {
        let configured = Config::load_user()
            .map(|c| c.storage.max_session_size)
            .unwrap_or_else(|_| ByteSize(256 * 1024 * 1024));
        Some(args.max_size.unwrap_or(configured).bytes())
    };

    let registry = source::driver_registry();
    let (driver, spec) = match source::resolve_source(&registry, args.source.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("gage dataset add: {e}");
            std::process::exit(1);
        }
    };
    let source_handle = match driver.open_source(&spec) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gage dataset add: {spec}: {e}");
            std::process::exit(1);
        }
    };

    // Confirm every selected session is present in the opened source
    // before writing anything, so one missing session leaves the
    // dataset empty rather than partially filled.
    let mut ids: Vec<String> = Vec::with_capacity(selected.len());
    let mut errors = 0;
    for info in &selected {
        match source_handle.find_native(&info.id) {
            Ok(id) => ids.push(id),
            Err(e) => {
                eprintln!("gage dataset add: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let mut natives = Vec::with_capacity(ids.len());
    for id in &ids {
        match source_handle.open_native(id) {
            Ok(s) => natives.push(s),
            Err(e) => {
                eprintln!("gage dataset add: {id}: {e}");
                std::process::exit(1);
            }
        }
    }

    let specs: Vec<SessionSpec<'_>> = natives
        .iter_mut()
        .map(|session| SessionSpec {
            driver: driver.as_ref(),
            session: session.as_mut(),
        })
        .collect();
    let datasets = match max_bytes {
        Some(max) => DatasetStore::from(&store).with_max_session_size(max),
        None => DatasetStore::from(&store),
    };
    let outcomes = match datasets.sessions_add(&dataset_id, specs) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gage dataset add: {e}");
            std::process::exit(1);
        }
    };
    for (outcome, id) in outcomes.iter().zip(&ids) {
        print_add_outcome(&outcome.outcome, &outcome.id, id, Some(&dataset_id));
    }

    if let Err(e) = source_handle.close() {
        eprintln!("gage dataset add: {spec}: {e}");
        std::process::exit(1);
    }
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
