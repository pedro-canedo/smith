//! Built-in tools (file, shell), the ToolRegistry, and the permission gate.

pub mod ask_user;
pub mod checkpoint;
pub mod chromium;
pub mod fs_tools;
pub mod grep;
pub mod registry;
pub mod shell_tool;
pub mod staging;
pub mod web_search;
pub mod write_tasks;

pub use checkpoint::CheckpointStore;
pub use registry::{DuplicateToolName, ToolRegistry};
