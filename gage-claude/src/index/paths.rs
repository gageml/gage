//! The paths a tool call refers to, read from its input: every
//! argument the harness names as a path, and every path-shaped token
//! of a shell command. A write the harness did not record, such as a
//! heredoc, is found through these; the judged writes are
//! `files_changed`.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// The paths `input` refers to, resolved against `cwd` and `home`,
/// each once, sorted. A Bash command contributes its tokens; any other tool contributes the string values of keys
/// ending in `path`. A token is path-shaped when it holds a `/`, no
/// URL scheme, and no glob character. A relative path with no `cwd`
/// is dropped, as is a `~` path with no `home`.
pub(crate) fn referenced_paths(
    name: &str,
    input: &Value,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push = |token: &str| {
        if let Some(path) = resolve(token, cwd, home)
            && !out.contains(&path)
        {
            out.push(path);
        }
    };
    if name == "Bash" {
        if let Some(command) = input.get("command").and_then(Value::as_str) {
            for token in command_tokens(command) {
                push(token);
            }
        }
    } else {
        path_values(input, &mut |s| push(s));
    }
    out.sort();
    out
}

/// The string values of `value` under keys ending in `path`, at any
/// depth.
fn path_values(value: &Value, f: &mut dyn FnMut(&str)) {
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                match v {
                    Value::String(s) if key.to_ascii_lowercase().ends_with("path") => f(s),
                    _ => path_values(v, f),
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                path_values(v, f);
            }
        }
        _ => {}
    }
}

/// The words of a shell command: split on whitespace, quotes, and the
/// separators that end a word in any shell (`;|&()<>=`), so that
/// `VAR=/p`, `--flag=/p`, and `>/p` each yield `/p`.
fn command_tokens(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(|c: char| c.is_whitespace() || "'\"`;|&()<>=".contains(c))
        .filter(|t| !t.is_empty())
}

/// `token` as a path when it is path-shaped, resolved and normalized.
fn resolve(token: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let token = token.trim_end_matches(['.', ',', ':', ';', ')']);
    if !token.contains('/')
        || token.contains("://")
        || token.contains(['*', '?', '['])
        || token.starts_with('-')
        || token.bytes().all(|b| b == b'/')
    {
        return None;
    }
    let path = if let Some(rest) = token.strip_prefix("~/") {
        home?.join(rest)
    } else if let Some(rest) = token.strip_prefix("$HOME/") {
        home?.join(rest)
    } else if token.starts_with('/') {
        PathBuf::from(token)
    } else {
        cwd?.join(token)
    };
    Some(normalize(&path))
}

/// `path` with `.` removed and `..` applied lexically
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(name: &str, input: Value) -> Vec<PathBuf> {
        referenced_paths(
            name,
            &input,
            Some(Path::new("/w/proj")),
            Some(Path::new("/home/u")),
        )
    }

    #[test]
    fn a_shell_command_yields_its_path_shaped_tokens_resolved() {
        let input = json!({"command":
            "cd src && cat > ~/dprint.jsonc <<'EOF'\n{}\nEOF\n\
             cp $HOME/a.txt ./b/../c.txt; ls -la /tmp/x/; \
             curl https://h/p/q; grep -r 'fn/x' *.rs --include=*/y; OUT=/out/f.log"});
        assert_eq!(
            paths("Bash", input),
            [
                "/home/u/a.txt",
                "/home/u/dprint.jsonc",
                "/out/f.log",
                "/tmp/x",
                "/w/proj/c.txt",
                "/w/proj/fn/x",
            ]
            .map(PathBuf::from),
            "a bare word such as the cd target is not path-shaped"
        );
    }

    #[test]
    fn other_tools_yield_their_path_arguments_at_any_depth() {
        let input = json!({"file_path": "/w/proj/a.rs", "old_string": "a/b", "new_string": "c/d",
                           "edits": [{"notebook_path": "nb/x.ipynb"}], "pattern": "**/*.rs"});
        assert_eq!(
            paths("Edit", input),
            ["/w/proj/a.rs", "/w/proj/nb/x.ipynb"].map(PathBuf::from)
        );
    }

    #[test]
    fn unresolvable_and_shapeless_tokens_are_dropped() {
        assert!(
            referenced_paths("Bash", &json!({"command": "cat a/b ~/c"}), None, None).is_empty()
        );
        assert!(paths("Bash", json!({"command": "echo / // --x/y a*b/c"})).is_empty());
        assert_eq!(
            paths("Bash", json!({"command": "see /a/b/c."})),
            [PathBuf::from("/a/b/c")]
        );
    }
}
