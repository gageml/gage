pub mod host;
pub mod server;
pub mod service;
pub mod tool;
pub mod tools;

pub use host::{HostError, McpHost, ServiceHandle};
pub use server::{GageServer, serve_stdio};
pub use service::{
    CustomToolCallback, CustomToolDef, GageTool, IssueWriteConfig, NoteWriteConfig, QueryConfig,
    ToolSpec, ToolsConfig, build_mcp_service,
};
