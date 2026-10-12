//! Rules for user-facing help text, checked over the whole clap
//! command tree. Every violation is collected before the test fails
//! so one run reports the full picture.

use clap::{ArgAction, Command, CommandFactory};

/// Widest rendered short-help line, in columns
const MAX_WIDTH: usize = 79;

/// Lines allowed past `MAX_WIDTH`, as (command, rendered line prefix).
/// `attachment add` has a 27-column flag column because of
/// `--stored <ATTACHMENT>`, which leaves too little room for this
/// text at its natural length.
const WIDTH_EXCEPTIONS: &[(&str, &str)] = &[("gage attachment add", "-r, --root <DIR>")];

/// Implementation vocabulary that must not reach a user
const INTERNAL_TERMS: &[&str] = &[
    "tree-ish",
    "refs",
    "attach task",
    "scan task",
    "object graph",
    "no-op",
    "fsck",
    "tombstone",
];

#[test]
fn help_text_follows_the_rules() {
    let mut root = crate::Cli::command();
    root.build();
    let mut violations = Vec::new();
    walk(&mut root, "gage", &mut violations);
    if !violations.is_empty() {
        violations.sort();
        panic!(
            "{} help text violations\n{}",
            violations.len(),
            violations.join("\n")
        );
    }
}

fn walk(cmd: &mut Command, path: &str, out: &mut Vec<String>) {
    *cmd = std::mem::take(cmd).term_width(400);
    let rendered = cmd.render_help().to_string();
    check_rendered(&rendered, path, out);
    if let Some(about) = cmd.get_about() {
        check_short(&about.to_string(), path, out);
    }
    if let Some(about) = cmd.get_long_about() {
        check_internal(&about.to_string(), &format!("{path} (long)"), out);
    }
    for arg in cmd.get_arguments() {
        let id = arg.get_id().as_str();
        if id == "help" || id == "version" {
            continue;
        }
        let label = match arg.get_long() {
            Some(long) => format!("--{long}"),
            None => format!("<{}>", arg.get_id().as_str().to_uppercase()),
        };
        let loc = format!("{path} {label}");
        if let Some(help) = arg.get_help() {
            let help = help.to_string();
            check_short(&help, &loc, out);
            if let ([value], true) = (arg.get_default_values(), arg.get_action().takes_values()) {
                let expected = format!("(default {})", value.to_string_lossy());
                if !help.contains(&expected) {
                    out.push(violation("default-value", &loc, &help));
                }
            }
            let repeats = matches!(arg.get_action(), ArgAction::Append | ArgAction::Count);
            let (_, topics) = split_trailing_parenthetical(&help);
            let delimited = arg.get_value_delimiter().is_some();
            if repeats && !arg.is_positional() && !delimited && !topics.contains(&"repeatable") {
                out.push(violation("repeat-missing", &loc, &help));
            }
        }
        if let Some(long) = arg.get_long_help() {
            let long = long.to_string();
            let summary = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
            if long != summary && !long.starts_with(&format!("{summary}\n\n")) {
                out.push(violation("long-summary", &loc, &long));
            }
            check_internal(&long, &format!("{loc} (long)"), out);
        }
    }
    for sub in cmd.get_subcommands_mut() {
        if sub.get_name() == "help" {
            continue;
        }
        let sub_path = format!("{path} {}", sub.get_name());
        walk(sub, &sub_path, out);
    }
}

fn check_rendered(rendered: &str, path: &str, out: &mut Vec<String>) {
    for line in rendered.lines() {
        let width = line.chars().count();
        let excepted = WIDTH_EXCEPTIONS
            .iter()
            .any(|(command, prefix)| *command == path && line.trim_start().starts_with(prefix));
        if line.starts_with("  ") && width > MAX_WIDTH && !excepted {
            out.push(violation("width", path, line.trim()));
        }
        if line.contains("[default:") {
            out.push(violation("clap-default", path, line.trim()));
        }
        if line.contains("[possible values:") {
            out.push(violation("clap-values", path, line.trim()));
        }
    }
}

fn check_short(text: &str, loc: &str, out: &mut Vec<String>) {
    if text.ends_with('.') {
        out.push(violation("period", loc, text));
    }
    if text.contains('`') || has_single_quoted(text) {
        out.push(violation("quote", loc, text));
    }
    if has_article(text) {
        out.push(violation("article", loc, text));
    }
    let (body, topics) = split_trailing_parenthetical(text);
    if body.contains(';') {
        out.push(violation("semicolon", loc, text));
    }
    if body.contains("default") || body.contains("repeat") {
        out.push(violation("topic-in-body", loc, text));
    }
    check_topics(&topics, loc, text, out);
    if body.contains("prefix") {
        out.push(violation("topic-in-body", loc, text));
    }
    check_internal(text, loc, out);
}

/// Splits `Body (a; b)` into the body and its topics; a text with no
/// trailing parenthetical has no topics
fn split_trailing_parenthetical(text: &str) -> (&str, Vec<&str>) {
    let Some(inner) = text.strip_suffix(')') else {
        return (text, Vec::new());
    };
    let Some(open) = inner.rfind('(') else {
        return (text, Vec::new());
    };
    let body = text[..open].trim_end();
    let topics = inner[open + 1..].split("; ").collect();
    (body, topics)
}

/// Topics appear in this order: value type, then `repeatable`, then
/// `default`, which runs to the end of its topic
const TOPIC_ORDER: &[&str] = &[
    "ID or prefix",
    "name, ID, or prefix",
    "tag, ID, or prefix",
    "path or name",
    "name or path",
    "glob",
    "repeatable",
    "default ",
];

const TYPE_TOPICS: usize = 6;

fn check_topics(topics: &[&str], loc: &str, text: &str, out: &mut Vec<String>) {
    let mut last_rank = 0;
    for topic in topics {
        let rank = TOPIC_ORDER
            .iter()
            .position(|known| topic == known || (known.ends_with(' ') && topic.starts_with(known)))
            .map(|rank| rank.max(TYPE_TOPICS - 1));
        match rank {
            Some(rank) if rank < last_rank => {
                out.push(violation("topic-order", loc, text));
            }
            Some(rank) => last_rank = rank,
            None => out.push(violation(&format!("topic:{topic}"), loc, text)),
        }
    }
}

fn has_article(text: &str) -> bool {
    text.split(|c: char| !c.is_alphabetic())
        .any(|word| matches!(word, "the" | "The"))
}

/// A `'` that opens a quoted word, as opposed to a possessive
fn has_single_quoted(text: &str) -> bool {
    text.starts_with('\'') || text.contains(" '") || text.contains("('")
}

fn check_internal(text: &str, loc: &str, out: &mut Vec<String>) {
    let lower = text.to_lowercase();
    for term in INTERNAL_TERMS {
        if lower.contains(term) {
            out.push(violation(&format!("internal-term:{term}"), loc, text));
        }
    }
}

fn violation(rule: &str, loc: &str, text: &str) -> String {
    let text = text.split('\n').next().unwrap_or_default();
    format!("{rule:<22} {loc:<40} {text}")
}
