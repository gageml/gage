//! The paths a tool call refers to, read from its input: every
//! argument the harness names as a path, and every path a shell
//! command names. A write the harness did not record, such as a
//! heredoc, is found through these; the collector types each path by
//! what the call's result proves about it.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// The paths `input` refers to, resolved against `cwd` and `home`,
/// each once, sorted. A Bash command contributes the paths it names;
/// any other tool contributes the string values of keys ending in
/// `path`, whatever their shape. A shell token that cannot be
/// resolved, a relative one with no `cwd` or a `~` one with no
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
            for path in command_paths(command, cwd, home) {
                push(Some(path));
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

/// The paths a shell command names, resolved and normalized. The
/// command is read in order as the shell runs it: a `cd` moves the
/// directory later relative paths resolve against, to the end of its
/// subshell, and an assigned variable is substituted where a later
/// token uses it. A token that uses a variable the command does not
/// assign, or that follows a `cd` to a directory the command does not
/// state, has no known path and is dropped.
///
/// A token that holds a `/` is a path when it is path-shaped. A token
/// without one is a path in two positions that name a file: the
/// target of a redirect, and a quoted string that ends in a file
/// extension, which is how an inline script names the file it opens.
/// Any other bare word is not: the text of a script is mostly dotted
/// names that are not files.
fn command_paths(command: &str, cwd: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
    let scripts = heredoc_bodies(command);
    let in_script = |at: usize| scripts.iter().any(|body| body.contains(&at));
    let tokens = command_tokens(command);
    let mut shell = Shell::new(cwd, home);
    let mut out = Vec::new();
    // The directory a `cd` enters, held until its target is read: the
    // target is itself a path in the directory the `cd` runs in.
    let mut entering: Option<Option<PathBuf>> = None;
    for (i, token) in tokens.iter().enumerate() {
        let entered = entering.take();
        let scripted = in_script(token.start);
        if !scripted {
            for (at, c) in token.gap.char_indices() {
                if !in_script(token.start - token.gap.len() + at) {
                    shell.scope(c);
                }
            }
            let next = tokens.get(i + 1);
            if token.text == "cd" && (i == 0 || token.gap.contains(COMMAND_ENDS)) {
                match next.filter(|n| !n.gap.contains(COMMAND_ENDS)) {
                    Some(target) => entering = Some(shell.dir(Some(target.text))),
                    None => shell.cwd = shell.dir(None),
                }
            } else if is_name(token.text) {
                match next.map(|n| n.gap) {
                    Some("=" | "=\"" | "='") => shell.assign(token.text, next.map(|n| n.text)),
                    Some(gap) if gap.starts_with('=') => shell.assign(token.text, None),
                    _ => {}
                }
            }
        }
        if let Some(text) = shell.expand(token.text) {
            let text = text.trim_end_matches(['.', ',', ':', ';', ')']);
            let names_file = if text.contains('/') {
                is_path_shaped(text)
            } else {
                (!scripted && token.gap.trim_end().ends_with('>') && is_file_word(text))
                    || (token.is_quoted() && has_extension(text))
            };
            if names_file && let Some(path) = absolute(text, shell.cwd.as_deref(), home) {
                out.push(path);
            }
        }
        if let Some(cwd) = entered {
            shell.cwd = cwd;
        }
    }
    out
}

/// The characters that end one shell command and start the next
const COMMAND_ENDS: [char; 5] = [';', '&', '|', '(', '\n'];

/// What a shell command has set so far as it is read: the directory
/// relative paths resolve against, and its variables.
struct Shell<'a> {
    /// `None` after a `cd` to a directory the command does not state
    cwd: Option<PathBuf>,
    home: Option<&'a Path>,
    vars: HashMap<String, String>,
    /// The directory to return to at the end of each open subshell
    scopes: Vec<Option<PathBuf>>,
}

impl<'a> Shell<'a> {
    fn new(cwd: Option<&Path>, home: Option<&'a Path>) -> Self {
        let mut vars = HashMap::new();
        if let Some(home) = home.and_then(Path::to_str) {
            vars.insert("HOME".to_string(), home.to_string());
        }
        Self {
            cwd: cwd.map(Path::to_path_buf),
            home,
            vars,
            scopes: Vec::new(),
        }
    }

    /// Open a subshell at `(` and close one at `)`, which restores the
    /// directory the subshell was opened in. A `)` with no subshell
    /// open, as in a `case` pattern, changes nothing.
    fn scope(&mut self, c: char) {
        match c {
            '(' => self.scopes.push(self.cwd.clone()),
            ')' => {
                if let Some(cwd) = self.scopes.pop() {
                    self.cwd = cwd;
                }
            }
            _ => {}
        }
    }

    /// The directory `cd target` enters; a bare `cd` goes home. `None`
    /// when the command does not state the directory.
    fn dir(&self, target: Option<&str>) -> Option<PathBuf> {
        match target {
            None | Some("~") => self.home.map(Path::to_path_buf),
            Some(target) => self
                .expand(target)
                .filter(|dir| !dir.starts_with('-'))
                .and_then(|dir| absolute(&dir, self.cwd.as_deref(), self.home)),
        }
    }

    /// Set `name` to `value` expanded; a value that is absent or
    /// cannot be expanded leaves the variable unknown.
    fn assign(&mut self, name: &str, value: Option<&str>) {
        match value.and_then(|v| self.expand(v)) {
            Some(value) => self.vars.insert(name.to_string(), value.into_owned()),
            None => self.vars.remove(name),
        };
    }

    /// `token` with each `$NAME` and `${NAME}` replaced by the
    /// variable's value; `None` when it uses a variable that is not
    /// known.
    fn expand<'t>(&self, token: &'t str) -> Option<Cow<'t, str>> {
        if !token.contains('$') {
            return Some(Cow::Borrowed(token));
        }
        let mut out = String::new();
        let mut rest = token;
        while let Some((before, after)) = rest.split_once('$') {
            out.push_str(before);
            let (name, tail) = match after.strip_prefix('{') {
                Some(braced) => braced.split_once('}')?,
                None => after.split_at(
                    after
                        .find(|c: char| !is_name_char(c))
                        .unwrap_or(after.len()),
                ),
            };
            out.push_str(self.vars.get(name)?);
            rest = tail;
        }
        out.push_str(rest);
        Some(Cow::Owned(out))
    }
}

/// The byte ranges of the command's heredoc bodies: the lines after a
/// `<<WORD` up to the line that is `WORD`. A body is the text of a
/// file or of a script in another language, not shell.
fn heredoc_bodies(command: &str) -> Vec<Range<usize>> {
    static OPENER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(<<<?)-?[ \t]*['"\\]?(\w+)"#).unwrap());
    let mut bodies = Vec::new();
    let mut from = 0;
    while let Some(opener) = OPENER.captures_at(command, from) {
        let (Some(whole), Some(operator), Some(word)) =
            (opener.get(0), opener.get(1), opener.get(2))
        else {
            break;
        };
        from = whole.end();
        // `<<<` is a here-string, which has no body
        if operator.as_str() == "<<<" {
            continue;
        }
        let Some(start) = command
            .get(from..)
            .and_then(|rest| rest.find('\n'))
            .map(|at| from + at + 1)
        else {
            break;
        };
        let mut end = start;
        for line in command
            .get(start..)
            .unwrap_or_default()
            .split_inclusive('\n')
        {
            if line.trim() == word.as_str() {
                break;
            }
            end += line.len();
        }
        bodies.push(start..end);
        from = end;
    }
    bodies
}

/// One word of a shell command
struct Token<'a> {
    text: &'a str,
    /// Byte offset of the word in the command
    start: usize,
    /// The separators between the previous word and this one
    gap: &'a str,
    /// The character after the word
    after: Option<char>,
}

impl Token<'_> {
    /// Whether one pair of quotes encloses the whole word
    fn is_quoted(&self) -> bool {
        let before = self.gap.chars().next_back();
        before == self.after && matches!(before, Some('\'' | '"'))
    }
}

/// The words of a shell command: split on whitespace, quotes, and the
/// separators that end a word in any shell (`;|&()<>=`), so that
/// `VAR=/p`, `--flag=/p`, and `>/p` each yield `/p`.
fn command_tokens(command: &str) -> Vec<Token<'_>> {
    let is_separator = |c: char| c.is_whitespace() || "'\"`;|&()<>=".contains(c);
    let mut tokens = Vec::new();
    let mut rest = command;
    let mut at = 0;
    loop {
        let gap_len = rest.find(|c| !is_separator(c)).unwrap_or(rest.len());
        let (gap, tail) = rest.split_at(gap_len);
        if tail.is_empty() {
            break;
        }
        let (text, tail) = tail.split_at(tail.find(is_separator).unwrap_or(tail.len()));
        tokens.push(Token {
            text,
            start: at + gap.len(),
            gap,
            after: tail.chars().next(),
        });
        at += gap.len() + text.len();
        rest = tail;
    }
    tokens
}

/// Whether `text` is a shell variable name.
fn is_name(text: &str) -> bool {
    text.chars().all(is_name_char) && !text.starts_with(|c: char| c.is_ascii_digit())
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether a token that holds a `/` is path-shaped: it holds no URL
/// scheme and no glob character, and is neither an option nor all
/// slashes.
fn is_path_shaped(token: &str) -> bool {
    !(token.contains("://")
        || token.contains(['*', '?', '['])
        || token.starts_with('-')
        || token.bytes().all(|b| b == b'/'))
}

/// Whether a token without a `/` can be the file a redirect writes: a
/// word of file name characters that is neither an option nor a
/// number, which a file descriptor or a comparison operand is.
fn is_file_word(token: &str) -> bool {
    !token.is_empty()
        && token.chars().all(is_file_name_char)
        && !token.starts_with('-')
        && !token.chars().all(|c| c.is_ascii_digit())
}

/// Whether a token without a `/` reads as a file name with an
/// extension: `Cargo.toml` and `a-b.tar.gz` do; `.rs`, `v0.3.1`, and
/// `e.to_string` do not.
fn has_extension(token: &str) -> bool {
    let Some((stem, extension)) = token.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && !token.starts_with(['.', '-'])
        && token.chars().all(is_file_name_char)
        && extension.len() <= 10
        && extension.chars().all(|c| c.is_ascii_alphanumeric())
        && extension.starts_with(|c: char| c.is_ascii_alphabetic())
}

fn is_file_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_.+-".contains(c)
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
                "/w/proj/src/c.txt",
                "/w/proj/src/fn/x",
            ]
            .map(PathBuf::from),
            "the cd target is not itself named, and later relative paths resolve against it"
        );
    }

    fn bash(command: &str) -> Vec<PathBuf> {
        paths("Bash", json!({"command": command}))
    }

    #[test]
    fn a_cd_moves_later_relative_paths_to_the_end_of_its_subshell() {
        assert_eq!(
            bash("cat ./a.rs; cd sub/dir && cat ./b.rs\ncd ..; cat ./c.rs"),
            [
                "/w/proj/a.rs",
                "/w/proj/sub/c.rs",
                "/w/proj/sub/dir",
                "/w/proj/sub/dir/b.rs"
            ]
            .map(PathBuf::from),
            "the cd target resolves against the directory the cd runs in"
        );
        assert_eq!(
            bash("(cd /other && cat ./in.rs); cat ./out.rs; x=$(cd sub; cat ./s.rs); cat ./t.rs"),
            [
                "/other",
                "/other/in.rs",
                "/w/proj/out.rs",
                "/w/proj/sub/s.rs",
                "/w/proj/t.rs"
            ]
            .map(PathBuf::from)
        );
        assert_eq!(
            bash("cd; cat ./h.rs; cd ~ && cat ./i.rs; echo cd ./not/a/dir; cat ./j.rs"),
            [
                "/home/u/h.rs",
                "/home/u/i.rs",
                "/home/u/j.rs",
                "/home/u/not/a/dir"
            ]
            .map(PathBuf::from),
            "cd is a command only where a command starts"
        );
    }

    #[test]
    fn a_path_after_a_cd_to_an_unstated_directory_is_dropped() {
        assert_eq!(
            bash("cd \"$(mktemp -d)\" && cat ./a.rs /abs/b.rs"),
            [PathBuf::from("/abs/b.rs")]
        );
        assert_eq!(
            bash("cd - && cat ./a.rs; cd $UNSET; cat ./b.rs; cd /known; cat ./c.rs"),
            ["/known", "/known/c.rs"].map(PathBuf::from)
        );
    }

    #[test]
    fn an_assigned_variable_is_substituted() {
        assert_eq!(
            bash("T=/tmp/t; D=\"$T/d\"; cd \"$T\" && cat ./a.rs ${D}/b.rs $HOME/c.rs $UNSET/e.rs"),
            [
                "/home/u/c.rs",
                "/tmp/t",
                "/tmp/t/a.rs",
                "/tmp/t/d",
                "/tmp/t/d/b.rs"
            ]
            .map(PathBuf::from),
            "a token using an unassigned variable is dropped"
        );
        assert_eq!(
            bash("T=/tmp/t; T=$(pwd); cat $T/a.rs"),
            [PathBuf::from("/tmp/t")],
            "a variable assigned a value the command does not state is unknown"
        );
    }

    #[test]
    fn a_redirect_target_without_a_slash_is_a_path() {
        assert_eq!(
            bash("cd sub && cat > out.py <<'EOF'\nx = a > b\nEOF\necho hi >>log 2>&1; echo 3 > 4"),
            ["/w/proj/sub/log", "/w/proj/sub/out.py"].map(PathBuf::from),
            "a redirect is read in the shell text, not in a heredoc body"
        );
    }

    #[test]
    fn a_quoted_file_name_without_a_slash_is_a_path() {
        assert_eq!(
            bash(
                "cd sub && python3 - <<'EOF'\nimport os.path\ncd elsewhere\n\
                 s = open('Cargo.toml').read()\n\
                 s.replace(\"e.to_string\", \"v0.3.1\").endswith(\".rs\")\n\
                 open(\"notes.tar.gz\", 'w').write(s)\nEOF\ncat ./after.rs"
            ),
            [
                "/w/proj/sub/Cargo.toml",
                "/w/proj/sub/after.rs",
                "/w/proj/sub/notes.tar.gz"
            ]
            .map(PathBuf::from),
            "a heredoc body is not shell: its cd is not a command"
        );
        assert_eq!(
            bash("python3 -c \"open('gen.py', 'w')\"; touch bare.rs"),
            [PathBuf::from("/w/proj/gen.py")],
            "an unquoted bare word is not a path"
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
