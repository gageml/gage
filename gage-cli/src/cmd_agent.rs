//! `gage agent` — UI surface over the `gage-agent` crate.

use clap::Args;

#[derive(Args)]
pub struct AgentArgs {
    /// Project slug used to archive the session under `~/.gage/claude/<project>/`
    #[arg(short = 'p', long = "project")]
    pub project: Option<String>,
}

pub fn run(args: AgentArgs) {
    match gage_agent::run(args.project) {
        Ok(status) => {
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        Err(e) => {
            eprintln!("gage agent: {e}");
            std::process::exit(1);
        }
    }
}
