//! `gage tag`: user-chosen names for objects in the Gage store.
//!
//! A tag names an object of any type. `add` resolves its object
//! argument as an object-ish: a tag name, an id, or a unique prefix.

use clap::{Args, Subcommand};
use gage_core::style::IdHighlighter;
use gage_core::uuid::short_uuid;
use gage_store::{Store, TagStore};
use tabled::{
    Table,
    settings::{
        Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::human::format_elapsed_ms;
use crate::style;

#[derive(Subcommand)]
pub enum TagCommand {
    /// Add a tag
    ///
    /// Names an object so that later commands can refer to it by the
    /// tag instead of its ID.
    Add(TagAddArgs),

    /// List tags
    List(TagListArgs),
}

#[derive(Args)]
pub struct TagAddArgs {
    /// Tag name
    ///
    /// Follows git's ref naming rules. '/' is allowed, so names can be
    /// grouped: 'garrett/baseline'.
    name: String,

    /// Object to tag
    ///
    /// A tag name, an object ID, or a unique ID prefix.
    object: String,

    /// Move the tag if it exists
    #[arg(short, long)]
    force: bool,
}

#[derive(Args)]
pub struct TagListArgs {
    /// Show only tags whose object is deleted
    #[arg(long)]
    dangling: bool,

    /// Show the full object ID
    #[arg(long)]
    full_id: bool,

    #[command(flatten)]
    limit: crate::limit::LimitArgs,
}

pub fn add(args: TagAddArgs) {
    let store = open_store("gage tag add");
    let added = match TagStore::from(&store).add(&args.name, &args.object, args.force) {
        Ok(added) => added,
        Err(e) => {
            eprintln!("gage tag add: {e}");
            std::process::exit(1);
        }
    };
    let type_name = added
        .object_type
        .strip_prefix("gage::")
        .unwrap_or(&added.object_type);
    let object = format!("{type_name} {}", short_uuid(&added.id));
    match added.previous_id {
        Some(previous) if previous == added.id => {
            println!("Tag {} already names {object}", added.name);
        }
        Some(previous) => {
            println!(
                "Moved tag {} from {} to {object}",
                added.name,
                short_uuid(&previous)
            );
        }
        None => println!("Tagged {object} as {}", added.name),
    }
}

pub fn list(args: TagListArgs) {
    let store = open_store("gage tag list");
    let tags = match TagStore::from(&store).records() {
        Ok(tags) => tags,
        Err(e) => {
            eprintln!("gage tag list: {e}");
            std::process::exit(1);
        }
    };
    let tags: Vec<_> = tags
        .into_iter()
        .filter(|t| !args.dangling || t.deleted)
        .collect();
    let total = tags.len();
    if total == 0 {
        if args.dangling {
            println!("No dangling tags found");
        } else {
            println!("No tags found");
        }
        return;
    }
    // Ids are highlighted against the same set an id argument resolves
    // against, so a shown prefix resolves
    let peers = match store.short_prefix_ids(None) {
        Ok(peers) => peers,
        Err(e) => {
            eprintln!("gage tag list: {e}");
            std::process::exit(1);
        }
    };
    let highlighter = IdHighlighter::new(peers);

    let shown = args.limit.show_count(total);
    let header: Vec<String> = ["Name", "Type", "Id", "Modified"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = tags.iter().take(shown).map(|tag| {
        let type_name = tag
            .object_type
            .strip_prefix("gage::")
            .unwrap_or(&tag.object_type);
        let type_cell = if tag.deleted {
            format!("{type_name} (deleted)")
        } else {
            type_name.to_string()
        };
        vec![
            tag.name.clone(),
            type_cell,
            if args.full_id {
                highlighter.full(&tag.id)
            } else {
                highlighter.short(&tag.id)
            },
            tag.modified_ms.map(format_elapsed_ms).unwrap_or_default(),
        ]
    });
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), style::tty(Color::FG_BRIGHT_YELLOW))
        .modify(
            Columns::one(0).not(Rows::first()),
            style::tty(Color::FG_BRIGHT_CYAN),
        )
        .modify(Columns::new(3..4).not(Rows::first()), style::dim())
        .to_string();
    println!("{table}");

    args.limit.print_summary(shown, total, "tag");
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
