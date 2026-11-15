//! 把 MCP server 的工具接到 agent(design-part2 §6,D-049、D-050)。
//!
//! 第一部分的 `ErasedTool` 就是接點(D-043):MCP 工具的參數是 JSON、schema 是對方給的,剛好就是抹除型別的那一層。
//!
//! server 給的所有東西都不可信:
//! - **名稱**:模型看到的是 `<server>__<tool>`(例如 `notes__search`),不同 server 的同名工具不會撞在一起。
//! - **說明**:會進模型的 prompt(可能藏 injection),控制字元換成空白、最多 1024 個字元。
//! - **schema**:啟動時編譯一次;編譯不過(不合法、用了外部 `$ref`、regex 語法不支援)那個工具就不提供。
//! - **只提供白名單裡的工具**(`select`):server 升級多出來的工具不會自動送給模型。

use std::sync::Arc;

use futures::future::BoxFuture;
use llm::ToolDef;
use mcp::{CallResult, McpClient, McpError, ToolList};
use serde_json::{Map, Value};

use crate::tool::{ErasedTool, ToolContext, ToolError};

/// 工具說明的上限(字元數)。
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
/// 參數不合法時,最多列出幾個問題(都列出來的話,一個很爛的參數可以產生很長的錯誤訊息)。
const MAX_REPORTED_PROBLEMS: usize = 3;

/// 呼叫 MCP 工具的那一端。正式的是 `McpClient`;測試換成假的,不必啟動 server。
pub trait CallTool: Send + Sync {
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        arguments: Map<String, Value>,
    ) -> BoxFuture<'a, Result<CallResult, McpError>>;
}

impl CallTool for McpClient {
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        arguments: Map<String, Value>,
    ) -> BoxFuture<'a, Result<CallResult, McpError>> {
        // `McpClient::call_tool(self, ..)`:指名呼叫固有方法,不然會解析成這個 trait 方法自己(無窮遞迴)。
        Box::pin(McpClient::call_tool(self, name, arguments))
    }
}

/// 一個 MCP 工具,包成 agent 的 `ErasedTool`。
pub struct McpTool {
    def: ToolDef,
    /// server 那邊的名稱(沒有前綴)。
    remote_name: String,
    /// 啟動時編譯好的 schema,每次呼叫重複用(編譯比驗證貴得多)。
    validator: jsonschema::Validator,
    // `Arc<dyn CallTool>`:同一個 server 的工具共用一個 client;Java 的話就是好幾個物件拿著同一個 client 的參考。
    caller: Arc<dyn CallTool>,
}

impl McpTool {
    /// `Err` 是給人看的原因(接在「the tool `x`」後面)。
    pub fn new(server: &str, tool: &mcp::Tool, caller: Arc<dyn CallTool>) -> Result<Self, String> {
        let validator = jsonschema::draft202012::options()
            // `pattern` 用 `regex` crate:保證線性時間。預設的 fancy-regex 會回溯,server 給的 pattern 可以讓驗證卡住(ReDoS)。
            .with_pattern_options(jsonschema::PatternOptions::regex())
            .build(&tool.input_schema)
            .map_err(|e| format!("has an invalid input schema: {e}"))?;
        Ok(Self {
            def: ToolDef {
                name: tool_name(server, &tool.name),
                description: description(server, tool),
                input_schema: tool.input_schema.clone(),
            },
            remote_name: tool.name.clone(),
            validator,
            caller,
        })
    }

    /// 用 server 的 schema 驗證參數。問題最多列 `MAX_REPORTED_PROBLEMS` 個。
    fn validate(&self, arguments: &Value) -> Result<(), ToolError> {
        let problems: Vec<String> = self
            .validator
            .iter_errors(arguments)
            .map(|e| {
                // 錯在根上就不寫位置;錯在某個欄位就寫 `at /a`,模型比較知道要改哪裡。
                let path = e.instance_path().to_string();
                if path.is_empty() {
                    e.to_string()
                } else {
                    format!("at {path}: {e}")
                }
            })
            .collect();
        if problems.is_empty() {
            return Ok(());
        }
        let mut message: Vec<&str> = problems
            .iter()
            .take(MAX_REPORTED_PROBLEMS)
            .map(String::as_str)
            .collect();
        let more = problems.len().saturating_sub(MAX_REPORTED_PROBLEMS);
        let extra = format!("(and {more} more)");
        if more > 0 {
            message.push(&extra);
        }
        Err(ToolError::InvalidArgs(message.join("; ")))
    }
}

impl ErasedTool for McpTool {
    fn def(&self) -> ToolDef {
        self.def.clone()
    }

    fn call_json<'a>(
        &'a self,
        args_json: &'a str,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let value: Value = serde_json::from_str(args_json)
                .map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
            // 先確認是物件(同內建工具的 `parse_args`),再用 schema 驗證;不合法的參數不送給 server。
            if !value.is_object() {
                return Err(ToolError::InvalidArgs(
                    "arguments must be a JSON object".into(),
                ));
            }
            self.validate(&value)?;
            let Value::Object(arguments) = value else {
                return Err(ToolError::InvalidArgs(
                    "arguments must be a JSON object".into(),
                ));
            };
            // 工具自己回報的失敗(`isError`)與協定層的錯誤,都交回模型(design-part2 §4)。
            match self.caller.call_tool(&self.remote_name, arguments).await {
                Ok(CallResult {
                    text,
                    is_error: false,
                }) => Ok(text),
                Ok(CallResult {
                    text,
                    is_error: true,
                }) => Err(ToolError::Failed(text)),
                Err(e) => Err(ToolError::Failed(e.to_string())),
            }
        })
    }
}

/// 模型看到的名稱。是否符合 Anthropic 的規則(`^[a-zA-Z0-9_-]{1,64}$`)由 `Toolbox::new` 檢查:
/// MCP 允許 `.`、最長 128 字,組出來的名稱不合格時,啟動就失敗。
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{server}__{tool}")
}

/// server 給的說明:控制字元(含換行)換成空白、最多 `MAX_DESCRIPTION_CHARS` 個字元。
/// 沒有說明(或整理後是空的)時寫一句中性的話:`Toolbox` 不收空的說明。
fn description(server: &str, tool: &mcp::Tool) -> String {
    let cleaned: String = tool
        .description
        .as_deref()
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return format!("The tool `{}` of the MCP server `{server}`.", tool.name);
    }
    if cleaned.chars().count() <= MAX_DESCRIPTION_CHARS {
        return cleaned.to_owned();
    }
    let mut cut: String = cleaned.chars().take(MAX_DESCRIPTION_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// 依白名單挑出要給模型的工具,順序照白名單(design-part2 §6、§9-3)。
///
/// 白名單裡的工具 server 沒有提供、被拒絕、或 schema 編譯不過 → 啟動失敗;所有問題一次列出。
/// server 提供但白名單沒列的,不提供。
pub fn select(
    server: &str,
    list: &ToolList,
    allowed: &[String],
    caller: Arc<dyn CallTool>,
) -> Result<Vec<McpTool>, Vec<String>> {
    let mut tools = Vec::new();
    let mut problems = Vec::new();
    // 名稱來自我們自己的設定檔(可信),可以直接放進訊息;server 的原因已經在 mcp crate 整理成可印的字串。
    for name in allowed {
        if let Some(tool) = list.tools.iter().find(|t| &t.name == name) {
            match McpTool::new(server, tool, Arc::clone(&caller)) {
                Ok(tool) => tools.push(tool),
                Err(reason) => {
                    problems.push(format!("MCP server `{server}`: the tool `{name}` {reason}"));
                }
            }
        } else if let Some(rejected) = list
            .rejected
            .iter()
            .find(|r| r.name.as_deref() == Some(name.as_str()))
        {
            problems.push(format!(
                "MCP server `{server}` offers the tool `{name}`, but it was rejected: {}",
                rejected.reason
            ));
        } else {
            problems.push(format!(
                "MCP server `{server}` does not offer the tool `{name}`"
            ));
        }
    }
    if problems.is_empty() {
        Ok(tools)
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests;
