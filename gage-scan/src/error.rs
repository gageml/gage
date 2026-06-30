// Render an `Err` value a task returned to a human string. A task that
// returns an unexpected `Err` is a programming error, so we want
// debug-level detail. Must not Debug-format the raw Rune value: that
// dispatches the DEBUG_FMT protocol via the interface environment, which
// is not set here (we run after the VM has finished), turning every task
// error into an opaque "Missing interface environment". Downcast to the
// typed Error and use its Rust `Debug` instead — that is plain Rust
// formatting, not the Rune protocol, so it is safe post-VM.
pub(crate) fn render_task_error(err: rune::runtime::Value) -> String {
    if let Ok(e) = rune::from_value::<gage_runtime::error::Error>(err.clone()) {
        format!("{e:?}")
    } else if let Ok(s) = rune::from_value::<String>(err.clone()) {
        s
    } else {
        format!(
            "task returned a non-error value of type `{}`",
            err.type_info()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gage_runtime::error::Error;

    // Guards the off-VM rendering path: a task's Err must render via the
    // typed Error's Rust Debug, never the Rune DEBUG_FMT protocol (which
    // would fail with "Missing interface environment" here).
    #[test]
    fn render_task_error_renders_typed_error() {
        let err = rune::to_value(Error::Args("bad field".to_string())).unwrap();
        assert_eq!(render_task_error(err), r#"Args("bad field")"#);
    }

    #[test]
    fn render_task_error_renders_plain_string() {
        let err = rune::to_value("boom".to_string()).unwrap();
        assert_eq!(render_task_error(err), "boom");
    }
}
