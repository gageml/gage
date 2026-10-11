//! Replacement `::std::io::print` and `::std::io::println` for Rune.
//!
//! Rune's stock implementations write to the process stdout. The
//! runtime installs these at the same path so `print!` and `println!`
//! in a task reach the task's output sink instead. Each `print(s)` call
//! becomes one [`Output::Print`] carrying `s` verbatim; each
//! `println(s)` becomes one [`Output::Println`] with no newline
//! appended. The consumer renders the newline.

use rune::{ContextError, Module};

use crate::{Output, send};

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate_item("std", ["io"])?;
    m.function("print", print).build()?;
    m.function("println", println).build()?;
    Ok(m)
}

fn print(s: &str) {
    send(Output::Print(s.to_string()));
}

fn println(s: &str) {
    send(Output::Println(s.to_string()));
}
