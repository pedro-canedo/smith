//! MCP client: server lifecycle management and bridging remote tools into smith_core::Tool.

pub mod bridge;
pub mod client;
mod transport;

pub use bridge::{namespaced_tool_name, McpToolAdapter};
pub use client::{McpClient, McpError, McpToolDef};
