//! Replacement `::std::io::print` and `::std::io::println` for Rune.
//!
//! Rune's stock implementations write directly to `std::io::stdout()`.
//! We install our own at the same path; they send the bytes through
//! the current task's `runtime_tx` as `Print` / `Println` worker
//! messages. The scheduler driver then forwards them as
//! [`crate::event::ScanEvent::Print`] / `Println` events.
//!
//! Each Rune `print(s)` call becomes one `Print { s }` event with `s`
//! exactly as the scanner passed it. Each `println(s)` becomes one
//! `Println { s }` (no trailing newline appended by the runtime —
//! `println` is the contract: "newline-terminated output". The
//! consumer renders accordingly.)

use rune::{ContextError, Module};

use crate::RuntimeOutput;
use crate::state::current_scan_ctx;

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate_item("std", ["io"])?;
    m.function("print", print_impl).build()?;
    m.function("println", println_impl).build()?;
    Ok(m)
}

fn print_impl(s: &str) {
    let ctx = current_scan_ctx();
    #[allow(clippy::let_underscore_must_use)]
    let _ = ctx.runtime_tx.send(RuntimeOutput::Print(s.to_string()));
}

fn println_impl(s: &str) {
    let ctx = current_scan_ctx();
    #[allow(clippy::let_underscore_must_use)]
    let _ = ctx.runtime_tx.send(RuntimeOutput::Println(s.to_string()));
}
