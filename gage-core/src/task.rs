//! Task name presentation.

/// Display form of a task name. Task names are Rune function
/// identifiers (snake case) while scanner names are kebab case;
/// presentation renders the task in kebab case to match. The
/// conversion is lossless: function names cannot contain hyphens.
pub fn task_name_display(task: &str) -> String {
    task.replace('_', "-")
}

/// Display form of a `{scanner}::{task}` pair.
pub fn task_display(scanner: &str, task: &str) -> String {
    format!("{scanner}::{}", task_name_display(task))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_task_renders_kebab() {
        assert_eq!(
            task_display("code-review", "project_summary"),
            "code-review::project-summary"
        );
    }

    #[test]
    fn single_word_task_unchanged() {
        assert_eq!(task_display("general", "findings"), "general::findings");
    }
}
