//! MCP serving for the new runtime's agents.
//!
//! A `call_agent` with tools registers one service per call on the
//! process-wide [`McpHost`] and hands the child the service's URL. A
//! service exposes the Gage tools and scanner-defined tools named in
//! its [`ToolSpec`]. The Gage tools read the data they serve from
//! their configuration; this crate holds no runtime state of its own.

pub mod host;
pub mod server;
pub mod service;
pub mod tool;
pub mod tools;

pub use host::{HostError, McpHost, ServiceHandle};
pub use rmcp::model::ToolAnnotations;
pub use service::{
    CustomToolCallback, CustomToolDef, CustomToolOutcome, GageTool, IssueWriteCallback,
    IssueWriteConfig, IssueWriteInput, QueryConfig, ToolSpec, ToolsConfig, build_mcp_service,
};
