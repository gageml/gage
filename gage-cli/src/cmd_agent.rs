//! `gage agent` — UI surface over the `gage-agent` crate.

use clap::Subcommand;

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Evaluate scanner evidence for issues
    Judge,
}

pub fn run(command: AgentCommand) {
    match command {
        AgentCommand::Judge => judge(),
    }
}

fn judge() {
    match gage_agent::judge() {
        Ok(status) => {
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        Err(e) => {
            eprintln!("gage agent judge: {e}");
            std::process::exit(1);
        }
    }
}
