//! MCP client: server lifecycle management over stdio, Streamable HTTP and
//! HTTP+SSE, and bridging remote tools, resources and prompts into smith.

pub mod bridge;
pub mod client;
pub mod http;
pub mod registry;
pub mod transport;
pub mod untrusted;

pub use bridge::{namespaced_tool_name, ListMcpResourcesTool, McpToolAdapter, ReadMcpResourceTool};
pub use client::{
    McpClient, McpError, McpPromptDef, McpPromptResult, McpResourceDef, McpToolDef,
    ServerCapabilities,
};
pub use registry::{ConnectedServer, McpRegistry, CONNECT_TIMEOUT};
pub use transport::{Transport, TransportKind};
