//! Rendering of the `Err` value a task returned. The renderer lives
//! in `gage-runtime` beside the typed `Error`; the publish engine
//! renders a filter task's error the same way.

pub use gage_runtime::error::render_task_error;

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
