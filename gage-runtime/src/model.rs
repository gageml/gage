//! Model alias resolution for `call_agent(...).model(...)`. Maps
//! short-form aliases (`small`/`medium`/`large` and the
//! family names `haiku`/`sonnet`/`opus`) to the concrete claude
//! model id passed through to `claude --model`.

const MODEL_ALIASES: &[(&str, &str)] = &[
    ("opus", "claude-opus-4-8"),
    ("large", "claude-opus-4-8"),
    ("sonnet", "claude-sonnet-4-6"),
    ("medium", "claude-sonnet-4-6"),
    ("haiku", "claude-haiku-4-5"),
    ("small", "claude-haiku-4-5"),
];

pub fn resolve_model(input: &str) -> &str {
    for (alias, target) in MODEL_ALIASES {
        if *alias == input {
            return target;
        }
    }
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_aliases() {
        assert_eq!(resolve_model("opus"), "claude-opus-4-8");
        assert_eq!(resolve_model("large"), "claude-opus-4-8");
        assert_eq!(resolve_model("sonnet"), "claude-sonnet-4-6");
        assert_eq!(resolve_model("medium"), "claude-sonnet-4-6");
        assert_eq!(resolve_model("haiku"), "claude-haiku-4-5");
        assert_eq!(resolve_model("small"), "claude-haiku-4-5");
    }

    #[test]
    fn passes_through_concrete_ids() {
        assert_eq!(resolve_model("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(resolve_model("unknown-model"), "unknown-model");
    }
}
