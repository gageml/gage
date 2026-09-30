use std::sync::{Arc, Mutex};

use clap::{Args, Subcommand};
use cliclack as cli;
use datafusion::arrow::array::{Int64Array, StringArray, TimestampMillisecondArray};
use gage_core::config::{ByteSize, Config};
use gage_core::datetime::ms_to_iso8601;
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
use crate::dialog::{self, DialogError};
use crate::human::format_elapsed_ms;
use crate::session_select::SessionSelectArgs;
use crate::style::{self, IdKind, styled_id};

#[derive(Subcommand)]
pub enum DatasetCommand {
    /// Add a dataset
    ///
    /// Without session-selection options, adds an empty dataset. With
    /// options, adds a dataset populated with the selected native
    /// sessions in a single step.
    Add(DatasetAddArgs),

    /// Re-add each dataset session from its source
    ///
    /// Every member is re-read from the source and re-added: a
    /// session whose native content is unchanged is a no-op, and a
    /// session whose native content grew is updated in place.
    /// Equivalent to running 'gage session add -d DATASET' against
    /// every member's native id.
    Refresh(DatasetRefreshArgs),

    /// List datasets
    List(DatasetListArgs),

    /// Show a dataset
    Show(DatasetShowArgs),

    /// Delete datasets
    ///
    /// Member sessions and scan runs that used a dataset are kept
    /// unless --sessions is given.
    Delete(DatasetDeleteArgs),
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
pub struct DatasetRefreshArgs {
    /// Dataset ID (or prefix)
    pub dataset: String,

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
}

#[derive(Args)]
pub struct DatasetListArgs {
    #[command(flatten)]
    limit: crate::limit::LimitArgs,
}

#[derive(Args)]
pub struct DatasetShowArgs {
    /// Dataset ID (or prefix)
    dataset: String,
}

#[derive(Args)]
pub struct DatasetDeleteArgs {
    /// Dataset IDs (or prefixes)
    #[arg(required = true)]
    ids: Vec<String>,

    /// Also remove member sessions from the store
    ///
    /// A session that another dataset holds is kept. Scan runs that
    /// used the dataset are not affected.
    #[arg(long)]
    sessions: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub async fn add(args: DatasetAddArgs) {
    let store = open_store("gage dataset add");
    let dataset_id = create_dataset("gage dataset add", &store);
    println!("Created dataset {}", short_uuid(&dataset_id));

    if args.select.is_empty() {
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

    populate_dataset(
        "gage dataset add",
        &store,
        &dataset_id,
        args.source.as_deref(),
        &args.select,
        max_bytes,
    )
    .await;
}

pub async fn refresh(args: DatasetRefreshArgs) {
    let store = open_store("gage dataset refresh");
    let datasets = DatasetStore::from(&store);
    let record = match datasets.get(&args.dataset) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gage dataset refresh: {}: {e}", args.dataset);
            std::process::exit(1);
        }
    };
    let members = match datasets.sessions_list(&record.id) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("gage dataset refresh: {e}");
            std::process::exit(1);
        }
    };
    if members.is_empty() {
        println!(
            "Dataset {} has no sessions to refresh",
            short_uuid(&record.id)
        );
        return;
    }

    // Look each member's native id back up on disk. A member whose
    // native session is gone is a hard error; the dataset stays
    // untouched in that case.
    let mut selected = Vec::with_capacity(members.len());
    let mut errors = Vec::new();
    for member in &members {
        match gage_claude::session::one_session(&member.native_id) {
            Ok(info) => selected.push(info),
            Err(e) => errors.push(e.to_string()),
        }
    }
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("gage dataset refresh: {e}");
        }
        std::process::exit(1);
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

    add_native_to_dataset(
        "gage dataset refresh",
        &store,
        &record.id,
        args.source.as_deref(),
        &selected,
        max_bytes,
    );
}

/// Create an empty dataset in `store`. Prints `command: <error>`
/// and exits on failure.
pub(crate) fn create_dataset(command: &str, store: &Store) -> String {
    match DatasetStore::from(store).create() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    }
}

/// Resolve `select` to native sessions and add them to
/// `dataset_id` through the opened source. Prints one
/// `Added session … (native …) to dataset …` line per added
/// session. On any failure it prints `command: <error>` and exits.
/// A selection that resolves to no sessions is announced and
/// otherwise is not an error.
pub(crate) async fn populate_dataset(
    command: &str,
    store: &Store,
    dataset_id: &str,
    source: Option<&str>,
    select: &SessionSelectArgs,
    max_bytes: Option<u64>,
) {
    let selected = match select.resolve(command).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    };
    if selected.is_empty() {
        println!("No sessions matched the selection; dataset is empty");
        return;
    }
    add_native_to_dataset(command, store, dataset_id, source, &selected, max_bytes);
}

/// Add each already-resolved native session record to `dataset_id`
/// through the opened source in a single commit. Prints outcome
/// lines and exits on any failure. `sessions_add` is idempotent per
/// session — a member whose native content is unchanged is a no-op,
/// a member whose content grew is updated in its slot, and a
/// non-member is appended.
pub(crate) fn add_native_to_dataset(
    command: &str,
    store: &Store,
    dataset_id: &str,
    source: Option<&str>,
    selected: &[gage_claude::session::SessionInfo],
    max_bytes: Option<u64>,
) {
    let registry = crate::source::driver_registry();
    let (driver, spec) = match crate::source::resolve_source(&registry, source) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    };
    let source_handle = match driver.open_source(&spec) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{command}: {spec}: {e}");
            std::process::exit(1);
        }
    };

    // Confirm every selected session is present in the opened
    // source before writing anything, so one missing session leaves
    // the dataset unchanged rather than partially updated.
    let mut ids: Vec<String> = Vec::with_capacity(selected.len());
    let mut errors = 0;
    for info in selected {
        match source_handle.find_native(&info.id) {
            Ok(id) => ids.push(id),
            Err(e) => {
                eprintln!("{command}: {e}");
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
                eprintln!("{command}: {id}: {e}");
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
        Some(max) => DatasetStore::from(store).with_max_session_size(max),
        None => DatasetStore::from(store),
    };
    let outcomes = match datasets.sessions_add(dataset_id, specs) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    };
    for (outcome, id) in outcomes.iter().zip(&ids) {
        print_add_outcome(&outcome.outcome, &outcome.id, id, Some(dataset_id));
    }

    if let Err(e) = source_handle.close() {
        eprintln!("{command}: {spec}: {e}");
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
        "SELECT d.id, d.id_prefix, d.created, \
         COALESCE(a.n, 0) AS attachments, COALESCE(m.n, 0) AS sessions \
         FROM dataset d \
         LEFT JOIN (SELECT dataset_id, COUNT(*) AS n FROM dataset_attachment \
                    GROUP BY dataset_id) a ON a.dataset_id = d.id \
         LEFT JOIN (SELECT dataset_id, COUNT(*) AS n FROM dataset_session \
                    GROUP BY dataset_id) m ON m.dataset_id = d.id \
         ORDER BY d.modified DESC LIMIT {show}"
    );
    let batches = run_query(&ctx, &sql).await;

    let header: Vec<String> = ["Id", "Sessions", "Attachments", "Created"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in &batches {
        let ids = column::<StringArray>(batch, 0);
        let prefixes = column::<StringArray>(batch, 1);
        let createds = column::<TimestampMillisecondArray>(batch, 2);
        let attachments = column::<Int64Array>(batch, 3);
        let sessions = column::<Int64Array>(batch, 4);
        for i in 0..batch.num_rows() {
            rows.push(vec![
                styled_id(short_uuid(ids.value(i)), prefixes.value(i), IdKind::Gage),
                sessions.value(i).to_string(),
                attachments.value(i).to_string(),
                format_elapsed_ms(createds.value(i)),
            ]);
        }
    }
    let shown = rows.len();
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::new(1..3), Alignment::right())
        .modify(Columns::new(1..).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");
    args.limit.print_summary(shown, total, "dataset");
}

pub fn show(args: DatasetShowArgs) {
    let store = open_store("gage dataset show");
    let datasets = DatasetStore::from(&store);
    let record = match datasets.get(&args.dataset) {
        Ok(record) => record,
        Err(e) => {
            eprintln!("gage dataset show: {}: {e}", args.dataset);
            std::process::exit(1);
        }
    };
    fn or_exit<T>(result: Result<T, gage_store::StoreError>) -> T {
        result.unwrap_or_else(|e| {
            eprintln!("gage dataset show: {e}");
            std::process::exit(1)
        })
    }
    let sessions = or_exit(datasets.sessions(&record.id));
    let attachments = or_exit(datasets.attachments(&record.id));
    let header = or_exit(store.read_header(&record.commit_sha));
    let iso = |ms: Option<i64>| ms.map(ms_to_iso8601).unwrap_or_default();

    let sessions_cell = sessions
        .iter()
        .map(|s| s.id.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let attachments_cell = attachments
        .iter()
        .map(|a| format!("{} {}", a.id, a.attrs.name))
        .collect::<Vec<_>>()
        .join("\n");
    let rows = [
        ("id", record.id.clone()),
        ("sessions", sessions_cell),
        ("attachments", attachments_cell),
        ("created", iso(Some(record.created_ms))),
        ("modified", iso(header.modified_ms)),
    ];
    let table = Table::from_iter(rows.iter().map(|(k, v)| [k.to_string(), v.clone()]))
        .with(Style::rounded())
        .modify(Columns::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .to_string();
    println!("{table}");
}

pub fn delete(args: DatasetDeleteArgs) {
    let store = open_store("gage dataset delete");
    let datasets = DatasetStore::from(&store);

    // Resolve every argument before writing anything, so one bad
    // argument leaves the store untouched
    let mut records = Vec::with_capacity(args.ids.len());
    let mut errors = 0;
    for prefix in &args.ids {
        match datasets.get(prefix) {
            Ok(record) => records.push(record),
            Err(e) => {
                eprintln!("gage dataset delete: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let count = records.len();
    let members: usize = records.iter().map(|r| r.session_count).sum();
    dialog::run("Delete datasets", || {
        let mut remark = format!("{count} {}", plural(count, "dataset"));
        if args.sessions {
            remark.push_str(&format!(", {members} {}", plural(members, "session")));
        }
        cli::log::remark(remark)?;

        if !args.yes {
            let action = if args.sessions {
                format!(
                    "Permanently delete {count} {} and remove their sessions?",
                    plural(count, "dataset")
                )
            } else {
                format!("Permanently delete {count} {}?", plural(count, "dataset"))
            };
            let confirmed = cli::confirm(format!("{action} This cannot be undone."))
                .initial_value(false)
                .interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let mut deleted = 0;
        let mut removed = 0;
        let mut kept = 0;
        for record in &records {
            let result = if args.sessions {
                datasets.delete_cascade(&record.id).map(|d| {
                    removed += d.removed.len();
                    kept += d.kept.len();
                })
            } else {
                datasets.delete(&record.id).map(|_| ())
            };
            match result {
                Ok(()) => deleted += 1,
                Err(e) => eprintln!("warning: failed to delete {}: {e}", short_uuid(&record.id)),
            }
        }

        let mut outro = format!("Deleted {deleted} {}", plural(deleted, "dataset"));
        if args.sessions {
            outro.push_str(&format!(
                ", removed {removed} {}",
                plural(removed, "session")
            ));
            if kept > 0 {
                outro.push_str(&format!(", kept {kept} held by other datasets"));
            }
        }
        Ok(outro.into())
    });
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        noun.to_string()
    } else {
        format!("{noun}s")
    }
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
