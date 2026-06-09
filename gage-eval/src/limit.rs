use clap::Args;

use tabled::{Table, settings::Style};

use crate::style;

const DEFAULT_LIMIT: usize = 20;

#[derive(Args)]
pub struct LimitArgs {
    /// Show more items (repeatable, adds LIMIT per flag)
    #[arg(short, long, action = clap::ArgAction::Count, conflicts_with = "all")]
    more: u8,

    /// Show all items
    #[arg(short, long, conflicts_with_all = ["more", "limit"])]
    all: bool,

    /// Limit the number of items shown (default: 20)
    #[arg(short, long, conflicts_with = "all")]
    limit: Option<usize>,
}

impl LimitArgs {
    pub fn show_count(&self, total: usize) -> usize {
        if self.all {
            total
        } else {
            let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
            (limit + limit * self.more as usize).min(total)
        }
    }

    pub fn print_summary(&self, show: usize, total: usize, noun: &str) {
        let plural = if total == 1 {
            noun.to_string()
        } else {
            format!("{noun}s")
        };
        let summary = if show < total {
            if self.limit.is_some() {
                format!("Showing {show} of {total} {plural}")
            } else {
                format!("Showing {show} of {total} {plural} (use -m for more, -a for all)")
            }
        } else {
            format!("Showing {show} {plural}")
        };
        let table = Table::from_iter([[summary]])
            .with(Style::empty())
            .modify(
                tabled::settings::object::Columns::first(),
                style::dim_italic(),
            )
            .to_string();
        println!("{table}");
    }
}
