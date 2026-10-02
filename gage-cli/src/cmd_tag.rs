//! `gage tag`: user-chosen names for objects in the Gage store.
//!
//! A tag names an object of any type. `add` resolves its object
//! argument as an object-ish: a tag name, an id, or a unique prefix.

use clap::{Args, Subcommand};
use gage_core::uuid::short_uuid;
use gage_store::{Store, TagStore};

#[derive(Subcommand)]
pub enum TagCommand {
    /// Add a tag
    ///
    /// Names an object so that later commands can refer to it by the
    /// tag instead of its ID.
    Add(TagAddArgs),
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
