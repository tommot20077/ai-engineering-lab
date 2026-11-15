//! `McpClient`:MCP 協定本身(design-part2 §4,規格 2026-07-28)。
//!
//! 2026-07-28 是無狀態的協定:沒有 `initialize` 握手、沒有 session。每個請求在 `params._meta` 自己帶上
//! 協定版本、client 能力、client 名稱,server 不記得前一個請求。所以「連線」只做一件事:用 `server/discover`
//! 探測(規格建議 stdio 的 client 這樣做),確認對方是新版 server、支援我們的版本、有提供工具。
//!
//! ```text
//! connect     server/discover ──→ supportedVersions 有 2026-07-28?capabilities 有 tools?
//! list_tools  tools/list(cursor)──→ …直到沒有 nextCursor(有頁數與工具數的上限)
//! call_tool   tools/call(name、arguments)──→ content[] 轉成一段文字、isError
//! ```
//!
//! server 送來的一切都是不可信輸入:結果逐欄檢查,要印出來的字串先整理過。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::McpError;
use crate::headers::{self, ParamHeader};
use crate::transport::Transport;

/// 唯一支援的協定版本(design-part2 §9-2:只支援驗證過的那一版)。
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// 2026-07-28 定義的錯誤碼(`-32020` ~ `-32099` 保留給 MCP 規格)。
pub const HEADER_MISMATCH: i64 = -32020;
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// `_meta` 的 key(規格 basic §_meta)。
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// 錯誤訊息裡最多列幾個 server 支援的版本、每個最多幾個字。
const MAX_LISTED_VERSIONS: usize = 10;
const MAX_VERSION_CHARS: usize = 32;
/// 被拒絕的工具名稱,印出來時最多幾個字。
const MAX_REJECTED_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientOptions {
    /// 每個請求的 `clientInfo`(給對方看、記 log 用;規格說不能拿來做安全判斷)。
    pub name: String,
    pub version: String,
    /// 每個請求等回應的上限。逾時的請求會被取消(傳輸送出 `notifications/cancelled`)。
    pub request_timeout: Duration,
    /// `tools/list` 最多翻幾頁、最多收幾個工具:server 可以一直給 `nextCursor`,不設上限就停不下來。
    pub max_pages: usize,
    pub max_tools: usize,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            name: "ai-engineering-lab".into(),
            // `env!`:編譯時讀 Cargo 設定的環境變數,值直接寫進程式(不是執行時讀環境變數)。
            version: env!("CARGO_PKG_VERSION").into(),
            request_timeout: Duration::from_secs(30),
            max_pages: 100,
            max_tools: 1000,
        }
    }
}

/// `server/discover` 告訴我們的 server 資訊。都是對方自己說的,只用來顯示。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerInfo {
    pub name: Option<String>,
    pub version: Option<String>,
    /// 給模型看的使用說明。不可信內容(可能藏 injection),要不要放進 prompt 由 agent 決定。
    pub instructions: Option<String>,
}

/// server 提供的一個工具。`description` 是原文,沒有整理過(agent 會去掉控制字元、截斷,design-part2 §6)。
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// JSON Schema,根的 `type` 一定是 `"object"`。
    pub input_schema: Value,
}

/// 格式不對、被略過的工具。`name` 是整理過、可以印的名稱(拿得到的話)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedTool {
    pub name: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolList {
    pub tools: Vec<Tool>,
    pub rejected: Vec<RejectedTool>,
}

/// 一次工具呼叫的結果:內容轉成一段文字,`is_error` 是工具自己回報的失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallResult {
    pub text: String,
    pub is_error: bool,
}

/// 一個 MCP server 的 client。
pub struct McpClient {
    // `Box<dyn Transport>`:stdio 或 HTTP,執行期依設定選(Java 的 `Transport transport` 欄位,介面型別)。
    transport: Box<dyn Transport>,
    options: ClientOptions,
    server: ServerInfo,
    /// `list_tools` 記下每個工具的 `x-mcp-header`,`call_tool` 照著加 `Mcp-Param-*` header。
    /// `Mutex`:`list_tools` 只拿 `&self`(呼叫端可以同時呼叫好幾個方法),要改內容就需要內部可變性(Java 的 synchronized 欄位)。
    param_headers: Mutex<HashMap<String, Vec<ParamHeader>>>,
}

impl McpClient {
    /// 用 `server/discover` 探測 server。失敗時關掉傳輸再回傳錯誤(子行程才會結束)。
    pub async fn connect(
        transport: Box<dyn Transport>,
        options: ClientOptions,
    ) -> Result<Self, McpError> {
        let mut client = Self {
            transport,
            options,
            server: ServerInfo::default(),
            param_headers: Mutex::new(HashMap::new()),
        };
        match client.discover().await {
            Ok(server) => {
                client.server = server;
                Ok(client)
            }
            Err(e) => {
                client.close().await;
                Err(e)
            }
        }
    }

    pub fn server(&self) -> &ServerInfo {
        &self.server
    }

    /// 列出所有工具(跟著 `nextCursor` 翻完每一頁)。格式不對的工具放進 `rejected`,不讓整份清單失效。
    pub async fn list_tools(&self) -> Result<ToolList, McpError> {
        const METHOD: &str = "tools/list";
        let invalid = |reason: String| McpError::InvalidResult {
            method: METHOD.into(),
            reason,
        };
        let mut list = ToolList::default();
        let mut param_headers = HashMap::new();
        let mut names = HashSet::new();
        let mut seen_cursors = HashSet::new();
        let mut cursor: Option<String> = None;
        for page in 1.. {
            if page > self.options.max_pages {
                return Err(invalid(format!(
                    "more than {} pages",
                    self.options.max_pages
                )));
            }
            let mut params = Map::new();
            if let Some(cursor) = &cursor {
                params.insert("cursor".into(), json!(cursor));
            }
            let mut result = self.request(METHOD, params, Vec::new()).await?;
            let Some(Value::Array(tools)) = result.remove("tools") else {
                return Err(invalid("`tools` must be an array".into()));
            };
            for tool in tools {
                match parse_tool(tool) {
                    // `HashSet::insert` 回傳「原本沒有」:名稱重複時保留第一個。
                    Ok((tool, headers)) if names.insert(tool.name.clone()) => {
                        param_headers.insert(tool.name.clone(), headers);
                        list.tools.push(tool);
                    }
                    Ok((tool, _)) => list.rejected.push(RejectedTool {
                        name: Some(tool.name),
                        reason: "duplicate name".into(),
                    }),
                    Err(rejected) => list.rejected.push(rejected),
                }
            }
            if list.tools.len() > self.options.max_tools {
                return Err(invalid(format!(
                    "more than {} tools",
                    self.options.max_tools
                )));
            }
            cursor = match result.remove("nextCursor") {
                None | Some(Value::Null) => break,
                Some(Value::String(next)) => Some(next),
                Some(_) => return Err(invalid("`nextCursor` must be a string".into())),
            };
            // 同一個 cursor 出現第二次:照著翻會繞圈。
            if let Some(next) = &cursor
                && !seen_cursors.insert(next.clone())
            {
                return Err(invalid("the server repeated a cursor".into()));
            }
        }
        *self
            .param_headers
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = param_headers;
        Ok(list)
    }

    /// 呼叫一個工具。`arguments` 由呼叫端驗證過(agent 用 server 給的 schema 驗證,design-part2 §6)。
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Map<String, Value>,
    ) -> Result<CallResult, McpError> {
        const METHOD: &str = "tools/call";
        let invalid = |reason: &str| McpError::InvalidResult {
            method: METHOD.into(),
            reason: reason.into(),
        };
        // `x-mcp-header`:沒列過的工具沒有記錄,就不帶(規格:server 回 HeaderMismatch 時,client 應該重新 tools/list)。
        let headers = self
            .param_headers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .map(|h| headers::param_values(h, &arguments))
            .unwrap_or_default();
        let mut params = Map::new();
        params.insert("name".into(), json!(name));
        params.insert("arguments".into(), Value::Object(arguments));
        let result = self.request(METHOD, params, headers).await?;
        let Some(Value::Array(content)) = result.get("content") else {
            return Err(invalid("`content` must be an array"));
        };
        // 沒有 `isError` 就是 false(規格)。
        let is_error = match result.get("isError") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(invalid("`isError` must be a boolean")),
        };
        Ok(CallResult {
            text: content_text(content, result.get("structuredContent")),
            is_error,
        })
    }

    /// 結束連線。
    pub async fn close(self) {
        self.transport.close().await;
    }

    async fn discover(&self) -> Result<ServerInfo, McpError> {
        const METHOD: &str = "server/discover";
        let result = match self.request(METHOD, Map::new(), Vec::new()).await {
            Ok(result) => result,
            // 規格(stdio「Backward Compatibility」):不是 2026-07-28 定義的錯誤、或一直不回,就是舊版 server。
            // 新版的錯誤(-32020 ~ -32022)原樣回報:對方是新版,只是不接受這個請求。
            Err(McpError::Rpc(e)) if !is_modern_error(e.code) => {
                return Err(McpError::NotModern(McpError::Rpc(e).to_string()));
            }
            Err(McpError::Timeout { .. }) => {
                return Err(McpError::NotModern(format!(
                    "no answer to `{METHOD}` within {:?}",
                    self.options.request_timeout
                )));
            }
            Err(e) => return Err(e),
        };
        let versions = match result.get("supportedVersions") {
            Some(Value::Array(items)) if items.iter().all(Value::is_string) => items,
            _ => {
                return Err(McpError::InvalidResult {
                    method: METHOD.into(),
                    reason: "`supportedVersions` must be an array of strings".into(),
                });
            }
        };
        if !versions.iter().any(|v| v == PROTOCOL_VERSION) {
            return Err(McpError::UnsupportedVersion {
                supported: printable_versions(versions),
            });
        }
        // `capabilities.tools` 要是物件(可以是空的 `{}`);沒有就是這個 server 沒有工具。
        let has_tools = result
            .get("capabilities")
            .and_then(|c| c.get("tools"))
            .is_some_and(Value::is_object);
        if !has_tools {
            return Err(McpError::NoTools);
        }
        let server_info = result.get("_meta").and_then(|m| m.get(META_SERVER_INFO));
        let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
        Ok(ServerInfo {
            name: text(server_info.and_then(|s| s.get("name"))),
            version: text(server_info.and_then(|s| s.get("version"))),
            instructions: text(result.get("instructions")),
        })
    }

    /// 送出一個請求:加上 `_meta`、套上逾時、檢查結果是物件、看 `resultType`。
    async fn request(
        &self,
        method: &str,
        mut params: Map<String, Value>,
        headers: Vec<(String, String)>,
    ) -> Result<Map<String, Value>, McpError> {
        params.insert(
            "_meta".into(),
            json!({
                META_VERSION: PROTOCOL_VERSION,
                // 不宣告任何能力(sampling、elicitation、roots 都不支援):server 就不會要求這些(規格 MRTR)。
                META_CAPABILITIES: {},
                META_CLIENT_INFO: {"name": self.options.name, "version": self.options.version},
            }),
        );
        let response = tokio::time::timeout(
            self.options.request_timeout,
            self.transport
                .request_with_headers(method, Some(Value::Object(params)), headers),
        )
        .await
        // 逾時:`timeout` 把請求的 future 丟掉,傳輸會把它從等待名單拿掉、送出取消通知。
        .map_err(|_| McpError::Timeout {
            method: method.into(),
        })?
        .map_err(classify)?;
        let invalid = |reason: &str| McpError::InvalidResult {
            method: method.into(),
            reason: reason.into(),
        };
        let Value::Object(result) = response else {
            return Err(invalid("the result is not a JSON object"));
        };
        // 規格:沒有 `resultType` 當成 "complete"(舊版 server 的格式);不認得的值一律不合法。
        match result.get("resultType") {
            None => Ok(result),
            Some(Value::String(t)) if t == "complete" => Ok(result),
            Some(Value::String(t)) if t == "input_required" => Err(McpError::InputRequired),
            Some(_) => Err(invalid("unknown resultType")),
        }
    }
}

fn is_modern_error(code: i64) -> bool {
    matches!(
        code,
        HEADER_MISMATCH | MISSING_REQUIRED_CLIENT_CAPABILITY | UNSUPPORTED_PROTOCOL_VERSION
    )
}

/// -32022 不管在哪個請求出現,都換成 `UnsupportedVersion`(附上對方支援的版本)。
fn classify(error: McpError) -> McpError {
    match error {
        McpError::Rpc(e) if e.code == UNSUPPORTED_PROTOCOL_VERSION => {
            McpError::UnsupportedVersion {
                supported: unsupported_versions(e.data.as_ref()),
            }
        }
        other => other,
    }
}

/// -32022 的 `data.supported`。
fn unsupported_versions(data: Option<&Value>) -> Vec<String> {
    match data.and_then(|d| d.get("supported")) {
        Some(Value::Array(items)) => printable_versions(items),
        _ => Vec::new(),
    }
}

/// server 給的版本字串會印進錯誤訊息:只留字串、整理成可印的、限制個數與長度。
fn printable_versions(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(Value::as_str)
        .take(MAX_LISTED_VERSIONS)
        .map(|v| printable(v, MAX_VERSION_CHARS))
        .collect()
}

/// 控制字元換成 `?`、最多 `max` 個字元(以字元計,不會切壞 UTF-8)。
fn printable(text: &str, max: usize) -> String {
    text.chars()
        .take(max)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// 檢查一個工具定義。名稱照規格的建議收緊成必須:名稱會進模型的 prompt,也會變成 agent 的工具名稱。
/// 一起回傳它的 `x-mcp-header`(標註不合法的工具,規格要求排除)。
fn parse_tool(value: Value) -> Result<(Tool, Vec<ParamHeader>), RejectedTool> {
    let Value::Object(mut tool) = value else {
        return Err(RejectedTool {
            name: None,
            reason: "a tool must be a JSON object".into(),
        });
    };
    let Some(Value::String(name)) = tool.remove("name") else {
        return Err(RejectedTool {
            name: None,
            reason: "`name` must be a string".into(),
        });
    };
    let reject = |reason: &str| RejectedTool {
        name: Some(printable(&name, MAX_REJECTED_NAME_CHARS)),
        reason: reason.into(),
    };
    if !valid_tool_name(&name) {
        return Err(reject("`name` must be 1 to 128 of A-Z a-z 0-9 _ - ."));
    }
    let input_schema = match tool.remove("inputSchema") {
        Some(schema) if schema.get("type").and_then(Value::as_str) == Some("object") => schema,
        _ => {
            return Err(reject(
                "`inputSchema` must be an object whose `type` is \"object\"",
            ));
        }
    };
    let param_headers = headers::param_headers(&input_schema).map_err(|reason| reject(&reason))?;
    // 選填的文字欄位:不是字串就當作沒有。
    let mut text = |key: &str| match tool.remove(key) {
        Some(Value::String(s)) => Some(s),
        _ => None,
    };
    let tool = Tool {
        title: text("title"),
        description: text("description"),
        name,
        input_schema,
    };
    Ok((tool, param_headers))
}

/// 規格(server/tools「Tool Names」):1~128 個字元,只有 ASCII 英數字、`_`、`-`、`.`。
fn valid_tool_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// `content` 轉成一段文字(design-part2 §4、§9-5):文字照順序一段一行;圖片、音訊、資源換成一行說明。
/// 一段文字都沒有、但有 `structuredContent` 時,接上它的 JSON(規格:有結構化結果的工具「應該」也給文字,但不保證)。
fn content_text(content: &[Value], structured: Option<&Value>) -> String {
    let mut lines = Vec::new();
    let mut has_text = false;
    for block in content {
        let line = match block.get("type").and_then(Value::as_str) {
            Some("text") => match block.get("text").and_then(Value::as_str) {
                Some(text) => {
                    has_text = true;
                    text.to_owned()
                }
                None => UNSUPPORTED.into(),
            },
            Some(kind @ ("image" | "audio")) => {
                let media_type = block.get("mimeType").and_then(Value::as_str);
                match media_type.filter(|m| valid_media_type(m)) {
                    Some(media_type) => format!("[{media_type} omitted]"),
                    None => format!("[{kind} omitted]"),
                }
            }
            Some("resource_link") => "[resource_link omitted]".into(),
            Some("resource") => "[resource omitted]".into(),
            _ => UNSUPPORTED.into(),
        };
        lines.push(line);
    }
    if !has_text && let Some(structured) = structured {
        lines.push(structured.to_string());
    }
    lines.join("\n")
}

const UNSUPPORTED: &str = "[unsupported content omitted]";

/// 像 `image/png`、`image/svg+xml` 的才放進說明:`類型/子類型`,只有英數字與 `.+-`,最多 64 個字元。
fn valid_media_type(media_type: &str) -> bool {
    let allowed = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
    };
    media_type.len() <= 64
        && media_type
            .split_once('/')
            .is_some_and(|(kind, sub)| allowed(kind) && allowed(sub))
}

#[cfg(test)]
mod tests;
