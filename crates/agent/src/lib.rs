//! Agent runtime(003,設計見 `docs/modules/003-agent-runtime/design.md`)。
//!
//! 模型說要用哪個工具、參數是什麼,程式去執行,把結果交回模型,直到模型給出答案。
//! 這一版有:工具介面(`Tool` / `ErasedTool` / `Toolbox`)、內建工具、agent loop(`Agent` / `Run`)、軌跡、CLI(`agent run`)。

pub mod cli;
pub mod config;
mod limits;
mod prompt;
mod run;
mod tool;
pub mod tools;
pub mod trajectory;

pub use limits::AgentLimits;
pub use prompt::DEFAULT_SYSTEM;
pub use run::{
    Agent, AgentConfig, AgentFailure, ConfigError, Finished, Limit, Outcome, Run, Step,
    ToolCallRecord,
};
pub use tool::{Clock, ErasedTool, Tool, ToolContext, ToolError, Toolbox, ToolboxError};
