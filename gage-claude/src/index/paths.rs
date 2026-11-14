//! The paths a tool call refers to, read from its input: every
//! argument the harness names as a path, and every path-shaped token
//! of a shell command. A write the harness did not record, such as a
//! heredoc, is found through these; the collector types each path by
//! what the call's result proves about it.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// The paths `input` refers to, resolved against `cwd` and `home`,
/// each once, sorted. A Bash command contributes its path-shaped
/// tokens; any other tool contributes the string values of keys
/// ending in `path`, whatever their shape. A shell token that cannot
/// be resolved, a relative one with no `cwd` or a `~` one with no
/// `home`, is dropped. A path argument that cannot be resolved is
/// kept as the record states it.
pub(crate) fn referenced_paths(
    name: &str,
    input: &Value,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push = |path: Option<PathBuf>| {
        if let Some(path) = path
            && !out.contains(&path)
        {
            out.push(path);
        }
    };
    if name == "Bash" {
        if let Some(command) = input.get("command").and_then(Value::as_str) {
            for token in command_tokens(command) {
                push(resolve_token(token, cwd, home));
            }
        }
    } else {
        path_values(input, &mut |s| push(resolve(s, cwd, home)));
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

/// A shell `token` as a path when it is path-shaped, resolved and
/// normalized. A token is path-shaped when it holds a `/`, no URL
/// scheme, and no glob character.
fn resolve_token(token: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let token = token.trim_end_matches(['.', ',', ':', ';', ')']);
    if !token.contains('/')
        || token.contains("://")
        || token.contains(['*', '?', '['])
        || token.starts_with('-')
        || token.bytes().all(|b| b == b'/')
    {
        return None;
    }
    absolute(token, cwd, home)
}

/// `path` resolved and normalized, or as the record states it when
/// nothing resolves it: a published session records
/// `[REDACTED]/src/lib.rs` and no working directory. The text is a
/// path by construction, a path argument or a recorded diff path, so
/// no shape is required of it: `app/[id]/page.tsx` holds glob
/// characters and is a file.
pub(crate) fn resolve(path: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    Some(absolute(path, cwd, home).unwrap_or_else(|| PathBuf::from(path)))
}

/// `path` as an absolute, normalized path; `None` for a relative path
/// with no `cwd` and a `~` path with no `home`.
fn absolute(path: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let resolved = if let Some(rest) = path.strip_prefix("~/") {
        home?.join(rest)
    } else if let Some(rest) = path.strip_prefix("$HOME/") {
        home?.join(rest)
    } else if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        cwd?.join(path)
    };
    Some(normalize(&resolved))
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

    /// A path argument is a path whatever characters it holds; the
    /// same text as a shell token is a glob.
    #[test]
    fn a_path_argument_keeps_glob_characters_a_shell_token_does_not() {
        assert_eq!(
            paths("Edit", json!({"file_path": "/w/proj/app/[id]/page.tsx"})),
            [PathBuf::from("/w/proj/app/[id]/page.tsx")]
        );
        assert_eq!(
            paths("Read", json!({"file_path": "[REDACTED]/src/what?.rs"})),
            [PathBuf::from("/w/proj/[REDACTED]/src/what?.rs")]
        );
        assert_eq!(
            paths("Grep", json!({"pattern": "x", "path": "src"})),
            [PathBuf::from("/w/proj/src")],
            "a relative argument without a slash is still a path"
        );
        assert!(paths("Read", json!({"file_path": ""})).is_empty());
        assert_eq!(
            referenced_paths(
                "Edit",
                &json!({"file_path": "[REDACTED]/src/lib.rs"}),
                None,
                None
            ),
            [PathBuf::from("[REDACTED]/src/lib.rs")],
            "an argument nothing resolves is kept as recorded"
        );
        assert!(paths("Bash", json!({"command": "cat /w/proj/app/[id]/page.tsx"})).is_empty());
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
