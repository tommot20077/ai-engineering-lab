//! 內建工具(design §7、D-044):都沒有副作用,不需要沙箱。另外是把 MCP server 的工具接進來的 `McpTool`(第二部分)。

mod calculator;
mod clock;
mod files;
pub mod mcp;

pub use calculator::{Calculator, CalculatorArgs};
pub use clock::{CurrentTime, CurrentTimeArgs};
pub use files::{ListDir, ListDirArgs, ReadFile, ReadFileArgs, Root};
pub use mcp::{McpTool, select as select_mcp_tools};
