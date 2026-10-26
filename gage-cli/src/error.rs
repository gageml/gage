//! The error a command returns. `main` prints it and sets the process
//! exit code.

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Command, CommandFactory};

use crate::Cli;

pub enum CliError {
    /// A deliberate exit. `message`, when present, prints to stderr; a
    /// command whose errors are already on screen exits without one.
    Exit { code: u8, message: Option<String> },
    /// A failure surfacing to the top. Prints as `{:#}`, so context
    /// added at the call site leads and the cause follows.
    Other(anyhow::Error),
    /// An argument clap accepted that the command rejects. Prints in
    /// clap's own layout and exits with clap's code.
    Usage(clap::Error),
}

impl CliError {
    /// A value of `option` that is not one of `valid`, reported the way
    /// clap reports a value outside a known set: the argument, the
    /// value, the valid values, and the nearest of them. `command` is
    /// the command path as help prints it, `gage publish init`, and
    /// `option` names the argument as its help does, `--filter` or
    /// `FILTER`. The valid set is the command's to decide at run time,
    /// so the argument itself declares none.
    pub fn invalid_value(command: &str, option: &str, value: &str, valid: &[&str]) -> Self {
        let mut root = Cli::command();
        root.build();
        let mut cmd: &Command = &root;
        for name in command.split_whitespace().skip(1) {
            match cmd.find_subcommand(name) {
                Some(sub) => cmd = sub,
                None => break,
            }
        }
        let arg = cmd
            .get_arguments()
            .find(|a| {
                a.get_long().is_some_and(|l| format!("--{l}") == option)
                    || a.get_value_names()
                        .is_some_and(|names| names.iter().any(|n| n == option))
            })
            .map_or_else(|| option.to_string(), ToString::to_string);
        let mut similar: Vec<(f64, &str)> = valid
            .iter()
            .map(|v| (strsim::jaro(value, v), *v))
            .filter(|(confidence, _)| *confidence > 0.7)
            .collect();
        similar.sort_by(|a, b| b.0.total_cmp(&a.0));

        let mut err = clap::Error::new(ErrorKind::InvalidValue).with_cmd(cmd);
        err.insert(ContextKind::InvalidArg, ContextValue::String(arg));
        err.insert(
            ContextKind::InvalidValue,
            ContextValue::String(value.to_string()),
        );
        err.insert(
            ContextKind::ValidValue,
            ContextValue::Strings(valid.iter().map(ToString::to_string).collect()),
        );
        if !similar.is_empty() {
            err.insert(
                ContextKind::SuggestedValue,
                ContextValue::Strings(similar.into_iter().map(|(_, v)| v.to_string()).collect()),
            );
        }
        Self::Usage(err)
    }

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
