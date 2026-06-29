pub mod host;
pub mod server;
pub mod tool;
pub mod tools;

pub use host::{HostError, McpHost, ServiceHandle};
pub use server::{GageServer, serve_stdio};
