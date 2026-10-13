use std::io::{self, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};

use cliclack as cli;
use console::{Term, style};

use crate::style as s;

struct GageTheme;

impl cli::Theme for GageTheme {
    fn default_progress_template(&self) -> String {
        s::PROGRESS_TEMPLATE.to_string()
    }

    fn progress_chars(&self) -> String {
        s::PROGRESS_CHARS.to_string()
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

pub fn run<F>(title: &str, f: F)
where
    F: FnOnce() -> Result<DialogResult, DialogError>,
{
    install_theme();
    let _sigint = SigintGuard::new();
    cli::intro(style(title).bold()).unwrap();
    handle_result(f());
}

pub async fn run_async<F, Fut>(title: &str, f: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<DialogResult, DialogError>>,
{
    install_theme();
    let _sigint = SigintGuard::new();
    cli::intro(style(title).bold()).unwrap();
    handle_result(f().await);
}

fn handle_result(result: Result<DialogResult, DialogError>) {
    match result {
        Ok(DialogResult::Message(msg)) => {
            cli::outro(style(msg).green().bright()).unwrap();
        }
        Err(DialogError::Interrupted) => {
            // Cliclack already showed cancel UI — nothing more to print
        }
        Err(DialogError::Canceled) => {
            cli::outro_cancel("Canceled").unwrap();
        }
        Err(DialogError::Failed(msg)) => {
            cli::outro_cancel(msg).unwrap();
            std::process::exit(1);
        }
        Err(DialogError::Other(e)) => {
            let wrapped = wrap_text(&Term::stderr(), &format!("{e:#}"))
                .lines()
                .map(|l| style(l).red().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            cli::log::error(wrapped).unwrap();
            cli::outro_cancel("Canceled due to errors").unwrap();
            std::process::exit(1);
        }
    }
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
