//! Session viewer entry point — loads the session document and runs
//! the viewer app.

use std::error::Error;
use std::io;

use ratatui::crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;

use crate::app;
use crate::options::ViewOptions;
use crate::session::Backend;

pub async fn run(
    backend: Backend,
    session_id: Option<&str>,
    options: ViewOptions,
) -> Result<(), Box<dyn Error>> {
    let document = match session_id {
        Some(id) => Some(backend.load(id).await?),
        None => None,
    };
    let mut terminal = ratatui::init();
    let enhanced_keys = push_keyboard_enhancements();
    let result = app::run(&mut terminal, document, &options, &backend);
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
pub(crate) fn push_keyboard_enhancements() -> bool {
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

pub(crate) fn pop_keyboard_enhancements() {
    if let Ok(()) = execute!(io::stdout(), PopKeyboardEnhancementFlags) {}
}
