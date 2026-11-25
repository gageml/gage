//! The user's desktop as a view reaches it: the clipboard and the web
//! browser.
//!
//! A view calls these through [`Desktop`] so that a test can record
//! the calls in place of a terminal and a browser.

use std::env;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

use crossterm::clipboard::CopyToClipboard;
use crossterm::execute;
use pulldown_cmark::{Event, Options, Parser, html};

pub(crate) trait Desktop: Send + Sync {
    /// Put `text` on the clipboard.
    fn copy_text(&self, text: &str) -> io::Result<()>;
    /// Whether a browser started here would appear in front of the
    /// user.
    fn can_browse(&self) -> bool;
    /// Show an HTML page in the user's browser.
    fn browse_html(&self, page: &str) -> io::Result<()>;
}

/// The desktop of the machine the process runs on.
pub(crate) struct SystemDesktop;

impl Desktop for SystemDesktop {
    /// Writes an OSC 52 sequence, which asks the terminal emulator to
    /// set its host's clipboard, so the copy reaches the user's
    /// machine over SSH. A terminal that does not support the sequence
    /// ignores it and reports nothing.
    fn copy_text(&self, text: &str) -> io::Result<()> {
        execute!(io::stdout(), CopyToClipboard::to_clipboard_from(text))
    }

    fn can_browse(&self) -> bool {
        display_is_local(|name| env::var_os(name).is_some_and(|v| !v.is_empty()))
    }

    fn browse_html(&self, page: &str) -> io::Result<()> {
        let mut file = tempfile::Builder::new()
            .prefix("gage-note-")
            .suffix(".html")
            .tempfile()?;
        file.write_all(page.as_bytes())?;
        // The browser reads the file after this call returns, so the
        // file outlives the process.
        let (_, path) = file.keep().map_err(|e| e.error)?;
        open_in_browser(&path)
    }
}

/// Whether the process has a display on the machine it runs on, given
/// a probe that says whether an environment variable is set. An SSH
/// session runs on a machine the user is not in front of, and a Linux
/// session without X11 or Wayland has no display to open a browser on.
fn display_is_local(is_set: impl Fn(&str) -> bool) -> bool {
    let over_ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .into_iter()
        .any(&is_set);
    if over_ssh {
        return false;
    }
    let needs_display_server = cfg!(all(unix, not(target_os = "macos")));
    !needs_display_server || is_set("DISPLAY") || is_set("WAYLAND_DISPLAY")
}

/// Start the platform's opener on `path`, detached from the terminal
/// so that its output cannot write over the view.
fn open_in_browser(path: &Path) -> io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        let mut command = Command::new("cmd");
        // The empty argument is the window title `start` expects
        // before a quoted path.
        command.args(["/c", "start", ""]);
        command
    } else {
        Command::new("xdg-open")
    };
    let opener = command.get_program().to_os_string();
    let mut child = command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", display(&opener))))?;
    // An opener can stay alive for as long as the browser it started,
    // so the wait runs off the event loop. The wait only reaps the
    // process: its result arrives after the caller has returned and
    // there is no dialog left to report it in.
    thread::spawn(move || drop(child.wait()));
    Ok(())
}

fn display(program: &OsStr) -> String {
    program.to_string_lossy().into_owned()
}

/// A standalone HTML page: `title`, a table of caption/value
/// attributes, and `markdown` rendered as the body. Raw HTML in the
/// markdown is shown as text, so that a page built from agent-written
/// content cannot run a script in the browser.
pub(crate) fn html_page(title: &str, attrs: &[(&str, &str)], markdown: &str) -> String {
    let options =
        Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let events = Parser::new_ext(markdown, options).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    let mut body = String::new();
    html::push_html(&mut body, events);

    let rows: String = attrs
        .iter()
        .map(|(caption, value)| {
            format!(
                "<tr><th>{}</th><td>{}</td></tr>\n",
                escape(caption),
                escape(value)
            )
        })
        .collect();
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>{title}</title>\n\
         <style>{STYLE}</style>\n</head>\n<body>\n<table class=\"attrs\">\n{rows}</table>\n\
         <hr>\n{body}</body>\n</html>\n",
        title = escape(title),
    )
}

const STYLE: &str = "\
body { font-family: system-ui, sans-serif; line-height: 1.5; max-width: 50rem; \
margin: 2rem auto; padding: 0 1rem; }
table { border-collapse: collapse; }
th, td { text-align: left; vertical-align: top; padding: 0.15rem 1rem 0.15rem 0; }
.attrs th { font-weight: normal; opacity: 0.65; }
.attrs td { white-space: pre-wrap; font-family: ui-monospace, monospace; }
pre { overflow-x: auto; padding: 0.75rem; border: 1px solid #8884; }
@media (prefers-color-scheme: dark) { body { background: #111; color: #ddd; } a { color: #8bf; } }
";

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ssh_session_has_no_local_display() {
        for name in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"] {
            assert!(
                !display_is_local(|var| var == name || var == "DISPLAY"),
                "{name}"
            );
        }
    }

    #[test]
    fn a_session_with_a_display_server_has_a_local_display() {
        assert!(display_is_local(|var| var == "DISPLAY"));
        assert!(display_is_local(|var| var == "WAYLAND_DISPLAY"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_linux_session_without_a_display_server_has_no_local_display() {
        assert!(!display_is_local(|_| false));
    }

    #[test]
    fn html_page_renders_the_attributes_and_the_markdown() {
        let page = html_page(
            "a <note>",
            &[("Name", "general"), ("Target", "session:a&b")],
            "# Heading\n\nSome *text*.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n",
        );
        assert!(page.contains("<title>a &lt;note&gt;</title>"), "{page}");
        assert!(page.contains("<tr><th>Name</th><td>general</td></tr>"));
        assert!(page.contains("<td>session:a&amp;b</td>"), "{page}");
        assert!(page.contains("<h1>Heading</h1>"), "{page}");
        assert!(page.contains("<em>text</em>"), "{page}");
        assert!(page.contains("<table>"), "{page}");
    }

    #[test]
    fn html_page_shows_raw_html_as_text() {
        let page = html_page(
            "note",
            &[],
            "Inline <script>alert(1)</script> here.\n\n<script>\nalert(2)\n</script>\n",
        );
        assert!(!page.contains("<script>"), "{page}");
        assert!(
            page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{page}"
        );
    }
}
