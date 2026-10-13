//! `gage tag`: user-chosen names for objects in the Gage store.
//!
//! A tag names an object of any type. Every argument that takes an
//! object id resolves an object-ish: a tag name, an id, or a unique
//! prefix.

use clap::{Args, Subcommand};
use cliclack as cli;
use gage_core::style::IdHighlighter;
use gage_core::uuid::short_uuid;
use gage_store::{Store, StoreError, TagStore};
use tabled::{
    Table,
    settings::{
        Color, Style,
        object::{Columns, Object, Rows},
    },
};

use crate::dialog::{self, DialogError};
use crate::human::{count, format_elapsed_ms};
use crate::style;

/// Long help shared by every option that names a new tag
pub const TAG_HELP: &str = "A tag name follows Git ref naming rules. An existing tag is an \
error unless --force-tag is given.";

#[derive(Subcommand)]
pub enum TagCommand {
    /// Add a tag
    ///
    /// Names an object so that later commands can refer to it by the
    /// tag instead of its ID.
    Add(TagAddArgs),

    /// List tags
    List(TagListArgs),

    /// Delete tags
    ///
    /// Removes the tags only. The objects they named are not changed.
    Delete(TagDeleteArgs),
}

#[derive(Args)]
pub struct TagAddArgs {
    /// Tag name
    ///
    /// Follows git's ref naming rules. '/' is allowed, so names can be
    /// grouped: 'garrett/baseline'.
    name: String,

    /// Object to tag (tag, ID, or prefix)
    ///
    /// A tag name, an object ID, or a unique ID prefix.
    object: String,

    /// Move tag if it exists
    #[arg(short, long)]
    force: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct TagListArgs {
    /// Show tags whose object is deleted
    #[arg(long)]
    orphaned: bool,

    #[command(flatten)]
    limit: crate::limit::LimitArgs,

    /// Show full object ID
    #[arg(long)]
    full_id: bool,
}

#[derive(Args)]
pub struct TagDeleteArgs {
    /// Tag names
    #[arg(required = true)]
    names: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub fn add(args: TagAddArgs) {
    let store = open_store("gage tag add");
    dialog::run("Add tag", || {
        let tags = TagStore::from(&store);
        let target = store
            .resolve_in(&args.object, None)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        let object = format!(
            "{} {}",
            type_name(&target.object_type),
            short_uuid(&target.id)
        );
        // An existing tag is an error unless --force; one that already
        // names the object is a no-op
        let previous = tags
            .resolve(&args.name)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        if let Some(p) = &previous {
            if p.id == target.id {
                return Ok(format!("Tag {} already names {object}", args.name).into());
            }
            if !args.force {
                let e = StoreError::TagExists {
                    name: args.name.clone(),
                    id: p.id.clone(),
                };
                return Err(DialogError::Failed(format!("{e} (use --force to move it)")));
            }
        }
        dialog::step_value("Tag", &args.name)?;
        dialog::step_value("Object", &object)?;
        if let Some(p) = &previous {
            let from = format!("{} {}", type_name(&p.object_type), short_uuid(&p.id));
            dialog::step_value("Moves from", &from)?;
        }
        if !args.yes && !cli::confirm("Continue?").initial_value(true).interact()? {
            return Err(DialogError::Canceled);
        }
        let added = tags
            .add(&args.name, &args.object, args.force)
            .map_err(|e| DialogError::Failed(e.to_string()))?;
        Ok(match added.previous_id {
            Some(previous) => format!(
                "Moved tag {} from {} to {object}",
                added.name,
                short_uuid(&previous)
            ),
            None => format!("Tagged {object} as {}", added.name),
        }
        .into())
    });
}

/// The bare type name of a `gage::` object type
fn type_name(object_type: &str) -> &str {
    object_type.strip_prefix("gage::").unwrap_or(object_type)
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
        .filter(|t| !args.orphaned || t.deleted)
        .collect();
    let total = tags.len();
    if total == 0 {
        if args.orphaned {
            println!("No orphaned tags found");
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

pub fn delete(args: TagDeleteArgs) {
    let store = open_store("gage tag delete");
    let tags = TagStore::from(&store);

    // Resolve every argument before deleting anything, so one bad
    // argument leaves the store untouched
    let mut lines: Vec<String> = Vec::with_capacity(args.names.len());
    let mut errors = 0;
    for name in &args.names {
        match tags.resolve(name) {
            Ok(Some(found)) => {
                let type_name = found
                    .object_type
                    .strip_prefix("gage::")
                    .unwrap_or(&found.object_type);
                let state = if found.deleted { " (deleted)" } else { "" };
                lines.push(format!(
                    "{name} -> {type_name} {}{state}",
                    short_uuid(&found.id)
                ));
            }
            Ok(None) => {
                eprintln!("gage tag delete: tag not found: {name}");
                errors += 1;
            }
            Err(e) => {
                eprintln!("gage tag delete: {name}: {e}");
                errors += 1;
            }
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }

    let total = args.names.len();
    dialog::run("Delete tags", || {
        dialog::step_list("Tags", lines)?;

        cli::log::remark("Tagged objects are unaffected")?;
        if !args.yes {
            let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
            if !confirmed {
                return Err(DialogError::Canceled);
            }
        }

        let mut deleted = 0;
        for name in &args.names {
            match tags.delete(name) {
                Ok(_) => deleted += 1,
                Err(e) => cli::log::warning(format!("{name}: {e}"))?,
            }
        }
        if deleted < total {
            return Err(dialog::partial("Deleted", deleted, total, "tag"));
        }
        Ok(format!("Deleted {}", count(deleted, "tag")).into())
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
