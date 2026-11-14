//! 工具介面(design §4、D-043)。
//!
//! 兩層:寫內建工具的人實作有型別的 `Tool`;agent loop 只看抹除型別的 `ErasedTool`。
//! 每個 `Tool` 自動是 `ErasedTool`(blanket impl);第二部分的 MCP 工具直接實作 `ErasedTool`。

use std::future::Future;

use futures::future::BoxFuture;
use jiff::Timestamp;
use llm::ToolDef;
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::de::DeserializeOwned;
use serde_json::Value;
use uuid::Uuid;

/// 有型別的工具。
pub trait Tool: Send + Sync {
    /// 參數型別:serde 解析成功就等於驗證通過;送給模型的 JSON Schema 也從它產生。
    type Args: DeserializeOwned + JsonSchema + Send;

    fn name(&self) -> &str;

    /// 給模型看的說明:做什麼、什麼時候該用。
    fn description(&self) -> &str;

    fn call(
        &self,
        args: Self::Args,
        ctx: &ToolContext,
    ) -> impl Future<Output = Result<String, ToolError>> + Send;
}

/// 抹除型別的工具:參數是模型給的原始 JSON 字串,可以放進 `Vec<Box<dyn ErasedTool>>`。
pub trait ErasedTool: Send + Sync {
    /// 送給模型的定義。
    fn def(&self) -> ToolDef;

    /// 解析參數、執行。參數不合法時回 `ToolError::InvalidArgs`,工具本身不會被呼叫。
    fn call_json<'a>(
        &'a self,
        args_json: &'a str,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<String, ToolError>>;
}

// blanket impl:「每一個 `Tool` 都是 `ErasedTool`」,一次寫完,不必每個工具各寫一份。
// 為什麼需要第二層:`Tool` 有關聯型別(`type Args`)、`call` 回傳 `impl Future`,
// 這種 trait 不能寫成 `dyn Tool`(compiler 不知道 Args 是什麼、future 有多大)。
// Java 對照:`List<Tool<?>>` 能裝參數型別不同的工具,因為泛型在執行期被抹除;
// Rust 的泛型在編譯期展開成各自的程式碼,要「抹除」就得自己寫這一層:參數一律是 JSON 字串、
// future 一律裝進 `Box`。
impl<T: Tool> ErasedTool for T {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: self.name().to_owned(),
            description: self.description().to_owned(),
            input_schema: schema_for::<T::Args>(),
        }
    }

    fn call_json<'a>(
        &'a self,
        args_json: &'a str,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<String, ToolError>> {
        // `async move { .. }` 產生一個 future,`Box::pin` 把它放到 heap 上、型別抹除成 `BoxFuture`。
        // parse, don't validate:`T::Args` 解析得出來,就是合法的參數;解析失敗,工具根本不會被呼叫。
        Box::pin(async move {
            let args = parse_args::<T::Args>(args_json)?;
            self.call(args, ctx).await
        })
    }
}

/// 解析模型給的參數。必須是 JSON 物件:跟送給模型的 schema(`"type": "object"`)一致。
// 為什麼不直接 `serde_json::from_str::<A>`:serde derive 出來的 struct 也接受 JSON 陣列,
// 照欄位宣告的順序填(`[1, 2]` 變成 `a = 1, b = 2`)。schema 說是物件,解析卻比 schema 寬鬆,
// 這正是 D-043 要避免的「兩邊不一致」。所以先解析成 `Value`、確認是物件,再轉成 `A`。
fn parse_args<A: DeserializeOwned>(args_json: &str) -> Result<A, ToolError> {
    let invalid = |e: serde_json::Error| ToolError::InvalidArgs(e.to_string());
    let value: Value = serde_json::from_str(args_json).map_err(invalid)?;
    if !value.is_object() {
        return Err(ToolError::InvalidArgs(
            "arguments must be a JSON object".into(),
        ));
    }
    serde_json::from_value(value).map_err(invalid)
}

/// 從參數型別產生送給模型的 JSON Schema。
fn schema_for<A: JsonSchema>() -> Value {
    // `with(|s| ..)`:拿到設定的可變參考,改幾個欄位(builder 的一種寫法)。
    let settings = SchemaSettings::draft2020_12().with(|s| {
        // 不要 `"$schema": "https://json-schema.org/..."`:模型用不到。
        s.meta_schema = None;
        // 巢狀的型別直接展開,不用 `$defs` + `$ref`:模型讀起來簡單,provider 也不必支援參照。
        s.inline_subschemas = true;
    });
    let mut schema = settings.into_generator().into_root_schema_for::<A>();
    // `title` 是 Rust 的型別名稱(`CurrentTimeArgs`),對模型是雜訊。
    schema.remove("title");
    schema.to_value()
}

/// 工具執行時拿得到的資訊。
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// 這次 agent 執行的 id。
    pub run_id: Uuid,
    /// 模型給這次工具呼叫的 id。之後有副作用的工具,接續執行時靠它避免做兩次(idempotency key)。
    pub call_id: String,
    pub clock: Clock,
}

/// 時鐘。測試和 eval 用固定的時間,答案才會每次一樣。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    System,
    Fixed(Timestamp),
}

impl Clock {
    pub fn now(&self) -> Timestamp {
        match self {
            Clock::System => Timestamp::now(),
            // `Fixed(at)` 從 `&self` 裡拿出來的 `at` 是 `&Timestamp`;`Timestamp` 是 `Copy`,`*at` 直接複製一份。
            Clock::Fixed(at) => *at,
        }
    }
}

/// 工具失敗。全部都會變成 `is_error: true` 的結果交回模型,讓模型自己修正。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("unknown tool {0:?}")]
    UnknownTool(String),
    #[error("{0}")]
    Failed(String),
}

/// 一組工具:建立時檢查,執行時依名稱找。
pub struct Toolbox {
    entries: Vec<Entry>,
}

struct Entry {
    def: ToolDef,
    tool: Box<dyn ErasedTool>,
}

impl Toolbox {
    /// 檢查每個工具的定義(寫入路徑 fail-loud);有任何問題就整組拒絕,一次列出全部。
    // 參數是 `Vec<Box<dyn ErasedTool>>`:每個工具的實際型別不同,裝箱之後才能放進同一個 Vec。
    // 呼叫端寫 `vec![Box::new(CurrentTime)]`,`Box<CurrentTime>` 會自動轉成 `Box<dyn ErasedTool>`。
    pub fn new(tools: Vec<Box<dyn ErasedTool>>) -> Result<Self, ToolboxError> {
        let entries: Vec<Entry> = tools
            .into_iter()
            .map(|tool| Entry {
                def: tool.def(),
                tool,
            })
            .collect();

        let mut problems = Vec::new();
        for (i, Entry { def, .. }) in entries.iter().enumerate() {
            // 名稱之後會出現在錯誤訊息裡;MCP server 給的名稱不可信,用 `{:?}` 印(會跳脫換行等字元)。
            let name = &def.name;
            if entries
                .iter()
                .take(i)
                .any(|earlier| earlier.def.name == *name)
            {
                problems.push(format!("tool name {name:?} is registered twice"));
            } else if !is_valid_name(name) {
                problems.push(format!(
                    "tool name {name:?} must be 1-64 characters of ASCII letters, digits, `_` or `-`"
                ));
            }
            if def.description.trim().is_empty() {
                problems.push(format!("tool {name:?} has an empty description"));
            }
            if def.input_schema.get("type") != Some(&Value::from("object")) {
                problems.push(format!(
                    r#"tool {name:?} must take a JSON object (its input schema needs "type": "object")"#
                ));
            }
        }

        if problems.is_empty() {
            Ok(Self { entries })
        } else {
            Err(ToolboxError { problems })
        }
    }

    /// 給 `Request.tools` 的定義,順序跟註冊時一樣(順序固定,prompt cache 才吃得到)。
    pub fn defs(&self) -> Vec<ToolDef> {
        self.entries.iter().map(|e| e.def.clone()).collect()
    }

    /// 依名稱找工具執行。找不到是模型的錯(叫了不存在的工具),回 `UnknownTool` 讓它修正。
    // 工具不多(十幾個以內),線性搜尋就好,不必另外建 HashMap。
    pub async fn call(
        &self,
        name: &str,
        args_json: &str,
        ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.def.name == name)
            .ok_or_else(|| ToolError::UnknownTool(name.to_owned()))?;
        entry.tool.call_json(args_json, ctx).await
    }
}

/// Anthropic 的規定:`^[a-zA-Z0-9_-]{1,64}$`。規則簡單,不必動用 regex。
fn is_valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 註冊的工具有問題。一次列出全部問題。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid tools:{}", bullets(.problems))]
pub struct ToolboxError {
    pub problems: Vec<String>,
}

fn bullets(problems: &[String]) -> String {
    problems.iter().map(|p| format!("\n  - {p}")).collect()
}

#[cfg(test)]
mod tests;
