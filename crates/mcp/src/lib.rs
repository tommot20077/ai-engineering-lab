//! MCP client(003 第二部分,設計見 `docs/modules/003-agent-runtime/design-part2.md`)。
//!
//! 從零實作(D-047):JSON-RPC 2.0 + MCP 的 tools,協定版本 2026-07-28(沒有 `initialize`,每個請求自己帶版本)。這個 crate 只管協定,不知道 agent 存在;
//! 把 MCP 工具接到 agent 的 `ErasedTool` 在 agent 那邊(design-part2 §9-1)。

pub mod client;
mod error;
mod headers;
pub mod jsonrpc;
pub mod transport;

pub use client::{CallResult, ClientOptions, McpClient, RejectedTool, ServerInfo, Tool, ToolList};
pub use error::McpError;
