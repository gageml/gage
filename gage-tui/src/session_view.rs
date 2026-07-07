//! Session viewer entry point — loads the session document and runs
//! the viewer app.

use std::error::Error;
use std::io;

use gage_db::db::open_db;
use ratatui::crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;

use crate::options::ViewOptions;
use crate::{app, session};

pub async fn run(session_id: &str, options: ViewOptions) -> Result<(), Box<dyn Error>> {
    let db = open_db()?;
    let document = session::load(session_id, &db).await?;
    let mut terminal = ratatui::init();
    let enhanced_keys = push_keyboard_enhancements();
    let result = app::run(&mut terminal, document, &options, &db);
    if enhanced_keys {
        pop_keyboard_enhancements();
    }
    ratatui::restore();
    result?;
    Ok(())
}

/// Enables the Kitty keyboard protocol so the input layer can distinguish
/// Shift+Enter from Enter. Terminals without support reject the escape
/// sequence; callers treat the failure as "stay on legacy input."
fn push_keyboard_enhancements() -> bool {
    let push = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );
    match push {
        Ok(()) => true,
        Err(_) => false,
    }
}

fn pop_keyboard_enhancements() {
    if let Ok(()) = execute!(io::stdout(), PopKeyboardEnhancementFlags) {}
}
