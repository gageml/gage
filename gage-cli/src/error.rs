//! The error a command returns. `main` prints it and sets the process
//! exit code.

pub enum CliError {
    /// A deliberate exit. `message`, when present, prints to stderr; a
    /// command whose errors are already on screen exits without one.
    Exit { code: u8, message: Option<String> },
    /// A failure surfacing to the top. Prints as `{:#}`, so context
    /// added at the call site leads and the cause follows.
    Other(anyhow::Error),
}

impl CliError {
    pub fn exit(code: u8) -> Self {
        Self::Exit {
            code,
            message: None,
        }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self::Exit {
            code: 1,
            message: Some(message.into()),
        }
    }
}

impl From<anyhow::Error> for CliError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}
