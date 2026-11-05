//! Terminal input for the event loops that block on a channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ratatui::crossterm::event::{self, Event};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Reads terminal input on a dedicated thread. `event::read` blocks,
/// so the thread polls with a timeout and checks `stop` between polls
/// to exit promptly once the view closes.
pub(crate) fn spawn_input_thread(stop: Arc<AtomicBool>) -> UnboundedReceiver<Event> {
    let (tx, rx) = unbounded_channel();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            // A poll/read error means terminal input is gone (closed
            // tty); the view exits on the dropped channel.
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
    rx
}
