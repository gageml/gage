//! Session viewer entry point — loads the session document and runs
//! the viewer app.

use std::error::Error;
use std::io;

use ratatui::crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;

use crate::app;
use crate::options::ViewOptions;
use crate::session::Backend;

/// Open the viewer on `session_id`, selected at JSONL line `line`
/// when one is given. Without a session the viewer opens its picker.
pub async fn run(
    backend: Backend,
    session_id: Option<&str>,
    line: Option<u32>,
    options: ViewOptions,
) -> Result<(), Box<dyn Error>> {
    let document = match session_id {
        Some(id) => Some(backend.load(id).await?),
        None => None,
    };
    let mut terminal = ratatui::try_init()?;
    let enhanced_keys = push_keyboard_enhancements();
    let result = app::run(
        &mut terminal,
        document,
        line,
        &options,
        &backend,
        enhanced_keys,
    );
    if enhanced_keys {
        pop_keyboard_enhancements();
    }
    ratatui::restore();
    result?;
    Ok(())
}

/// Enables the Kitty keyboard protocol when the terminal supports it,
/// so the input layer can distinguish Shift+Enter from Enter. Returns
/// whether the protocol is active; callers show the newline hint that
/// matches. Support is queried first because a terminal without the
/// protocol ignores the push sequence rather than rejecting it, so a
/// successful write proves nothing. The query must run before the
/// caller starts reading events: it reads the terminal's reply itself.
pub(crate) fn push_keyboard_enhancements() -> bool {
    // A failed query (no tty, or no reply within crossterm's timeout)
    // means the protocol is unusable here, which is the same outcome
    // as an unsupported terminal.
    let supported = matches!(supports_keyboard_enhancement(), Ok(true));
    if !supported {
        return false;
    }
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
