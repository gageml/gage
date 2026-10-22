use std::fmt::Display;
use std::io::{self, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};

use cliclack as cli;
use console::{Term, style};
use gage_core::style::SPINNER_FRAMES;

use crate::error::CliError;
use crate::human::count;
use crate::style as s;

struct GageTheme;

impl cli::Theme for GageTheme {
    fn default_progress_template(&self) -> String {
        s::PROGRESS_TEMPLATE.to_string()
    }

    fn progress_chars(&self) -> String {
        s::PROGRESS_CHARS.to_string()
    }

    /// indicatif takes the last character as the finished-state glyph
    /// and animates the rest, so a space follows the frames
    fn spinner_chars(&self) -> String {
        format!("{} ", SPINNER_FRAMES.concat())
    }

    fn format_footer_with_message(&self, state: &cli::ThemeState, message: &str) -> String {
        format!(
            "{}\n",
            self.bar_color(state).apply_to(match state {
                cli::ThemeState::Active => format!("└  {message}"),
                cli::ThemeState::Cancel => "│\n└  Canceled".to_string(),
                cli::ThemeState::Submit => "│".to_string(),
                cli::ThemeState::Error(err) => format!("└  {err}"),
            })
        )
    }
}

pub enum DialogResult {
    Message(String),
}

impl<S: Into<String>> From<S> for DialogResult {
    fn from(s: S) -> Self {
        DialogResult::Message(s.into())
    }
}

/// Dialog-specific error type that distinguishes between user
/// cancellation (said "no"), Ctrl+C interrupts, and other errors.
pub enum DialogError {
    /// User explicitly declined a prompt (e.g. answered "no" to confirm).
    Canceled,
    /// Ctrl+C pressed during an interactive prompt. Cliclack already
    /// renders its own cancel UI so no outro is needed.
    Interrupted,
    /// The operation failed for a reason already surfaced earlier in
    /// the dialog (e.g. per-task errors during a scan). The message
    /// goes straight to the outro line — no preceding `log::error`,
    /// since the detail was already printed.
    Failed(String),
    /// Any other error. Carries `anyhow::Error` so the full source
    /// chain (and any `.context(...)` added at the call site) survives
    /// to the renderer, which prints with `{:#}`.
    Other(anyhow::Error),
}

impl From<io::Error> for DialogError {
    fn from(e: io::Error) -> Self {
        if e.kind() == ErrorKind::Interrupted {
            DialogError::Interrupted
        } else {
            DialogError::Other(e.into())
        }
    }
}

impl From<anyhow::Error> for DialogError {
    fn from(e: anyhow::Error) -> Self {
        DialogError::Other(e)
    }
}

pub fn install_theme() {
    cli::set_theme(GageTheme);
}

/// Set by the first Ctrl+C a dialog receives
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// True once Ctrl+C was pressed during the dialog. A loop that writes
/// one object per iteration checks this between objects and stops with
/// the store consistent.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

/// The dialog's SIGINT handler. The first Ctrl+C records the request
/// and returns, so a prompt can surface `ErrorKind::Interrupted` and
/// a loop can stop at an object boundary. A second Ctrl+C restores the
/// default disposition and re-raises, which terminates the process.
extern "C" fn on_sigint(_: libc::c_int) {
    if INTERRUPTED.swap(true, Ordering::Relaxed) {
        // SAFETY: signal and raise are async-signal-safe
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::raise(libc::SIGINT);
        }
    }
}

/// RAII guard that routes SIGINT to [`on_sigint`] for its lifetime and
/// restores the previous disposition on drop. The console crate
/// detects Ctrl+C as a 0x03 byte in raw mode and then calls
/// `libc::raise(SIGINT)`; under the default disposition the raised
/// signal terminates the process before the caller can act on the
/// `ErrorKind::Interrupted` returned by the prompt.
pub struct SigintGuard {
    prev: libc::sighandler_t,
}

impl SigintGuard {
    pub fn new() -> Self {
        INTERRUPTED.store(false, Ordering::Relaxed);
        let handler = on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t;
        let prev = unsafe { libc::signal(libc::SIGINT, handler) };
        Self { prev }
    }
}

impl Drop for SigintGuard {
    fn drop(&mut self) {
        unsafe {
            libc::signal(libc::SIGINT, self.prev);
        }
    }
}

pub fn run<F>(title: &str, f: F) -> Result<(), CliError>
where
    F: FnOnce() -> Result<DialogResult, DialogError>,
{
    install_theme();
    let _sigint = SigintGuard::new();
    cli::intro(style(title).bold()).unwrap();
    handle_result(f())
}

pub async fn run_async<F, Fut>(title: &str, f: F) -> Result<(), CliError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<DialogResult, DialogError>>,
{
    install_theme();
    let _sigint = SigintGuard::new();
    cli::intro(style(title).bold()).unwrap();
    handle_result(f().await)
}

/// The dialog prints its own outcome, so a failure exits without a
/// message
fn handle_result(result: Result<DialogResult, DialogError>) -> Result<(), CliError> {
    match result {
        Ok(DialogResult::Message(msg)) => {
            cli::outro(style(msg).green().bright()).unwrap();
            Ok(())
        }
        Err(DialogError::Interrupted) => {
            // Cliclack already showed cancel UI — nothing more to print
            Ok(())
        }
        Err(DialogError::Canceled) => {
            cli::outro_cancel("Canceled").unwrap();
            Ok(())
        }
        Err(DialogError::Failed(msg)) => {
            cli::outro_cancel(msg).unwrap();
            Err(CliError::exit(1))
        }
        Err(DialogError::Other(e)) => {
            let wrapped = wrap_text(&Term::stderr(), &format!("{e:#}"))
                .lines()
                .map(|l| style(l).red().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            cli::log::error(wrapped).unwrap();
            cli::outro_cancel("Canceled due to errors").unwrap();
            Err(CliError::exit(1))
        }
    }
}

/// A step naming a set of objects: `label` on the first line, then
/// one dimmed line per object. The lines name each object the step
/// concerns, `<short id> <detail>`, so a confirmation that follows
/// refers to a visible list.
pub fn step_list(label: &str, lines: impl IntoIterator<Item = String>) -> io::Result<()> {
    step_lines(
        label,
        lines.into_iter().map(|line| style(line).dim().to_string()),
    )
}

/// A step whose lines carry their own styling
pub fn step_lines(label: &str, lines: impl IntoIterator<Item = String>) -> io::Result<()> {
    let mut text = label.to_string();
    for line in lines {
        text.push('\n');
        text.push_str(&line);
    }
    cli::log::step(text)
}

/// A step with one dimmed value line
pub fn step_value(label: &str, value: impl Display) -> io::Result<()> {
    step_list(label, [value.to_string()])
}

/// The failure of a loop that completed `done` of `total` objects:
/// `Deleted 2 of 3 notes`. The per-object failures were logged as
/// the loop ran, so the message goes straight to the outro.
pub fn partial(verb: &str, done: usize, total: usize, noun: &str) -> DialogError {
    DialogError::Failed(format!("{verb} {done} of {}", count(total, noun)))
}

/// Wrap long text for dialog log lines. Width is capped at 79 columns
/// regardless of terminal width, subject to a 40-column floor and the
/// 4-column cliclack gutter. `term` must be the stream the wrapped text
/// is ultimately written to, so the width matches that stream's size.
pub fn wrap_text(term: &Term, text: &str) -> String {
    let width = term.size().1 as usize;
    let width = width.saturating_sub(4).clamp(40, 79);
    textwrap::fill(text, width)
}
