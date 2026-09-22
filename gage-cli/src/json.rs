//! ANSI syntax coloring for pretty-printed JSON, for display inside a
//! `tabled` cell. The scheme mirrors the TUI's `styles::Syntax`: keys
//! cyan, strings green, numbers yellow, constants bold magenta,
//! punctuation unstyled.

use console::Style;

/// Pretty-print `value` with 2-space indentation and color its tokens.
pub fn render(value: &serde_json::Value) -> String {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    pretty
        .lines()
        .map(color_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Color the tokens of one line of pretty-printed JSON. Strings never
/// span lines in serde's output, so each line tokenizes on its own.
fn color_line(line: &str) -> String {
    let key = Style::new().cyan();
    let string = Style::new().green();
    let number = Style::new().yellow();
    let constant = Style::new().magenta().bold();

    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            let mut text = String::from(c);
            let mut escaped = false;
            for c in chars.by_ref() {
                text.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    break;
                }
            }
            let is_key = chars.clone().find(|c| !c.is_whitespace()) == Some(':');
            let style = if is_key { &key } else { &string };
            out.push_str(&style.apply_to(text).to_string());
        } else if c == '-' || c.is_ascii_digit() {
            let mut text = String::from(c);
            while let Some(&next) = chars.peek() {
                if next.is_ascii_digit() || matches!(next, '-' | '+' | '.' | 'e' | 'E') {
                    text.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push_str(&number.apply_to(text).to_string());
        } else if c.is_ascii_alphabetic() {
            let mut text = String::from(c);
            while let Some(&next) = chars.peek() {
                if next.is_ascii_alphabetic() {
                    text.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push_str(&constant.apply_to(text).to_string());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(styled: &str) -> String {
        console::strip_ansi_codes(styled).into_owned()
    }

    #[test]
    fn render_keeps_the_text_and_colors_tokens() {
        console::set_colors_enabled(true);
        let value = serde_json::json!({"k": "v: x", "n": -1.5e3, "t": true, "z": null});
        let out = render(&value);
        assert_eq!(plain(&out), serde_json::to_string_pretty(&value).unwrap());
        assert!(out.contains(&Style::new().cyan().apply_to("\"k\"").to_string()));
        assert!(out.contains(&Style::new().green().apply_to("\"v: x\"").to_string()));
        assert!(out.contains(&Style::new().yellow().apply_to("-1500.0").to_string()));
        assert!(out.contains(&Style::new().magenta().bold().apply_to("true").to_string()));
        assert!(out.contains(&Style::new().magenta().bold().apply_to("null").to_string()));
    }

    #[test]
    fn escaped_quotes_stay_inside_the_string() {
        console::set_colors_enabled(true);
        let value = serde_json::json!({"q": "say \"hi\""});
        let out = render(&value);
        assert_eq!(plain(&out), serde_json::to_string_pretty(&value).unwrap());
        assert!(
            out.contains(
                &Style::new()
                    .green()
                    .apply_to("\"say \\\"hi\\\"\"")
                    .to_string()
            )
        );
    }
}
