//! 內建工具(design §7、D-044)。另外是把 MCP server 的工具接進來的 `McpTool`(第二部分)。
//!
//! 第三部分加上有副作用的工具(`write_file`、`edit_file`):每次呼叫都要核准(`Tool::needs_approval`,D-056)。

mod calculator;
mod clock;
mod files;
pub mod mcp;
mod root;
mod write;

pub use calculator::{Calculator, CalculatorArgs};
pub use clock::{CurrentTime, CurrentTimeArgs};
pub use files::{ListDir, ListDirArgs, ReadFile, ReadFileArgs};
pub use mcp::{McpTool, select as select_mcp_tools};
pub use root::Root;
pub use write::{EditFile, EditFileArgs, WriteFile, WriteFileArgs};
