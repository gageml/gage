//! Shared per-session size cap for commands that store native
//! sessions. `SizeLimitArgs` flattens into a subcommand's clap struct;
//! `max_bytes` resolves the flags against the configured default.

use clap::Args;
use gage_core::config::{ByteSize, Config};

#[derive(Args)]
pub struct SizeLimitArgs {
    /// Maximum stored size per session
    ///
    /// Overrides the configured default. Accepts a byte count or a
    /// unit suffix, such as 256MB or 1GB. A session whose stored files
    /// exceed this is refused.
    #[arg(long, value_name = "SIZE", value_parser = parse_byte_size)]
    pub max_size: Option<ByteSize>,

    /// Add regardless of size
    ///
    /// Stores the session even when it exceeds the size limit.
    #[arg(long)]
    pub force: bool,
}

impl SizeLimitArgs {
    /// The cap in bytes, or `None` when --force lifts it. The flag
    /// overrides the configured default.
    pub fn max_bytes(&self) -> Option<u64> {
        if self.force {
            return None;
        }
        let configured = Config::load_user()
            .map(|c| c.storage.max_session_size)
            .unwrap_or_else(|_| ByteSize(256 * 1024 * 1024));
        Some(self.max_size.unwrap_or(configured).bytes())
    }
}

fn parse_byte_size(s: &str) -> Result<ByteSize, String> {
    s.parse()
}
