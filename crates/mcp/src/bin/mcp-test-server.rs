//! 測試用的 MCP server(design-part2 §7、§9-4,D-051)。
//!
//!     mcp-test-server --stdio [旗標…]
//!     mcp-test-server --http <port> [旗標…] [--token T] [--sse] [--sse-request] [--sse-truncate] [--redirect]
//!                                           [--text-response]
//!
//! 共用的旗標:`--versions V1,V2`、`--legacy`、`--no-tools`、`--silent-discover`、`--bad-tools`、`--loop-cursor`。
//! HTTP 模式綁 127.0.0.1;port 給 0 由系統挑,實際的 URL 印在 stdout 的第一行(測試讀它)。
//!
//! 整合測試用 `env!("CARGO_BIN_EXE_mcp-test-server")` 找到它。CI 不必裝 Node 或 Python,而且可以照劇本故意出錯
//! (真的 server 不會配合)。
//!
//! 刻意**不用** `mcp::jsonrpc` 解析與編碼,直接操作 `serde_json::Value`:server 跟 client 共用同一個 parser 的話,
//! parser 的 bug 兩邊對稱,測試就看不出來。
//!
//! 兩組方法:
//! - `test/*`:測試傳輸(第 2 步),例如並行、故意送壞掉的訊息、帶著 stderr 結束。
//! - MCP 本身(第 3 步,規格 2026-07-28):`server/discover`、`tools/list`(每頁 4 個)、`tools/call`。
//!   每個請求都檢查 `_meta`(版本、client 能力),跟真的 server 一樣。命令列參數讓它扮演各種不配合的 server。
//! - HTTP 模式(第 4 步)只有 MCP 的方法;另外檢查規格要求的 header(`MCP-Protocol-Version`、`Mcp-Method`、
//!   `Mcp-Name`、`Mcp-Param-*`),不一致就回 400 + -32020,跟真的 server 一樣嚴格。

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};
use tokio::sync::oneshot;

const USAGE: &str = "usage: mcp-test-server (--stdio | --http PORT) [--versions V1,V2] [--legacy] \
                     [--no-tools] [--silent-discover] [--bad-tools] [--loop-cursor] \
                     [--token T] [--sse] [--sse-request] [--sse-truncate] [--redirect] [--text-response]";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Some((Mode::Stdio, flags)) => stdio(flags).await,
        Some((Mode::Http(port), flags)) => http(port, flags).await,
        None => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

enum Mode {
    Stdio,
    Http(u16),
}

/// 命令列參數:讓 server 扮演各種不配合的 server。
#[derive(Debug, Default)]
struct Flags {
    /// 支援的協定版本。預設只有 2026-07-28。
    versions: Vec<String>,
    /// 舊版(2025-11-25 以前)的 server:不認得 `server/discover`。
    legacy: bool,
    /// capabilities 裡沒有 `tools`。
    no_tools: bool,
    /// 收到 `server/discover` 不回應(有些舊版 server 對 `initialize` 之前的請求就是這樣)。
    silent_discover: bool,
    /// `tools/list` 混進格式不對的工具。
    bad_tools: bool,
    /// `nextCursor` 永遠是同一個:不設上限的 client 會一直翻頁。
    loop_cursor: bool,
    // ---- 只有 HTTP 模式用 ----
    /// 要求 `Authorization: Bearer <token>`。
    token: Option<String>,
    /// 用 SSE 回應(先送一個通知,再送回應);預設回一份 JSON。
    sse: bool,
    /// SSE 串流裡先送一個 server 的請求(規格禁止)。
    sse_request: bool,
    /// SSE 串流送了通知就結束,沒有回應。
    sse_truncate: bool,
    /// 回 307 叫 client 改去 `/elsewhere`(client 不能跟過去)。
    redirect: bool,
    /// 回 200 但本文是 `text/plain`(不是 JSON 也不是 SSE)。
    text_response: bool,
}

fn parse_args(args: &[String]) -> Option<(Mode, Flags)> {
    let mut rest = args.iter();
    let mode = match rest.next()?.as_str() {
        "--stdio" => Mode::Stdio,
        "--http" => Mode::Http(rest.next()?.parse().ok()?),
        _ => return None,
    };
    let mut flags = Flags {
        versions: vec!["2026-07-28".into()],
        ..Flags::default()
    };
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--token" => flags.token = Some(rest.next()?.clone()),
            "--sse" => flags.sse = true,
            "--sse-request" => flags.sse_request = true,
            "--sse-truncate" => flags.sse_truncate = true,
            "--redirect" => flags.redirect = true,
            "--text-response" => flags.text_response = true,
            "--versions" => flags.versions = rest.next()?.split(',').map(String::from).collect(),
            "--legacy" => flags.legacy = true,
            "--no-tools" => flags.no_tools = true,
            "--silent-discover" => flags.silent_discover = true,
            "--bad-tools" => flags.bad_tools = true,
            "--loop-cursor" => flags.loop_cursor = true,
            _ => return None,
        }
    }
    Some((mode, flags))
}

/// 一個請求的處理結果。
enum Reply {
    Result(Value),
    Error(i64, String, Option<Value>),
    /// 不回應。
    Silent,
}

fn error(code: i64, message: impl Into<String>) -> Reply {
    Reply::Error(code, message.into(), None)
}

struct Server {
    /// 一則訊息要一次寫完一整行:多個請求並行處理,共用 stdout 時要排隊。
    /// 這裡用 tokio 的 `Mutex`:寫入要 `.await`,std 的鎖不能跨 `.await` 拿著。
    out: tokio::sync::Mutex<Stdout>,
    /// server 送給 client 的請求(`test/ask`),等 client 回應。key 是我們配的字串 id。
    waiting: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// `test/linger` 之後,stdin 關掉也不結束(測試 client 關閉時會不會 kill)。
    linger: AtomicBool,
    flags: Flags,
    /// 收到的 `notifications/cancelled` 裡的 `requestId`(`test/cancelled` 讀出來給測試看)。
    cancelled: Mutex<Vec<Value>>,
}

impl Server {
    async fn write_raw(&self, text: &str) {
        let mut out = self.out.lock().await;
        // 寫不出去代表 client 已經走了,測試 server 也沒什麼好做的。
        let _ = out.write_all(text.as_bytes()).await;
        let _ = out.flush().await;
    }

    async fn send(&self, message: &Value) {
        self.write_raw(&format!("{message}\n")).await;
    }

    async fn reply(&self, id: &Value, result: Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }

    fn was_cancelled(&self, id: &Value) -> bool {
        self.cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(id)
    }

    /// 處理一個請求。每個請求在自己的 task 裡跑,所以 `test/sleep` 不會擋住其他請求(測試回應的順序可以跟請求不同)。
    async fn handle(&self, id: Value, method: &str, params: Value) {
        let reply = match self.mcp(method, &params).await {
            Some(reply) => reply,
            None => self.test_method(method, params).await,
        };
        // 被取消的請求不再送任何訊息(規格:cancellation)。
        if self.was_cancelled(&id) {
            return;
        }
        match reply {
            Reply::Result(result) => self.reply(&id, result).await,
            Reply::Error(code, message, data) => {
                let mut error = json!({"code": code, "message": message});
                if let (Some(data), Some(object)) = (data, error.as_object_mut()) {
                    object.insert("data".into(), data);
                }
                self.send(&json!({"jsonrpc": "2.0", "id": id, "error": error}))
                    .await;
            }
            Reply::Silent => {}
        }
    }

    /// 傳輸用的 `test/*` 方法(第 2 步)。
    async fn test_method(&self, method: &str, params: Value) -> Reply {
        let result = match method {
            "ping" => json!({}),
            "test/echo" => params,
            "test/sleep" => {
                let ms = field(&params, "ms").as_u64().unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                field(&params, "value").clone()
            }
            // 只回名稱;值只回 `MCP_TEST_` 開頭的。測試失敗時,斷言訊息不會把 CI 的環境變數(可能有 key)印出來。
            "test/env" => {
                let mut names: Vec<String> = std::env::vars_os()
                    .map(|(name, _)| name.to_string_lossy().into_owned())
                    .collect();
                names.sort();
                let values: serde_json::Map<String, Value> = std::env::vars()
                    .filter(|(name, _)| name.starts_with("MCP_TEST_"))
                    .map(|(name, value)| (name, Value::String(value)))
                    .collect();
                json!({"names": names, "values": values})
            }
            "test/notify" => {
                self.send(&json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info"}}))
                    .await;
                json!("after the notification")
            }
            // 反過來問 client(sampling 之類):client 應該回 -32601。把收到的回應整個交回去給測試檢查。
            "test/ask" => {
                let (tx, rx) = oneshot::channel();
                self.waiting
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert("s-1".into(), tx);
                self.send(&json!({"jsonrpc": "2.0", "id": "s-1", "method": "sampling/createMessage", "params": {}}))
                    .await;
                rx.await.unwrap_or(Value::Null)
            }
            "test/big" => {
                let bytes = field(&params, "bytes").as_u64().unwrap_or(0);
                json!("x".repeat(usize::try_from(bytes).unwrap_or(0)))
            }
            // ---- 故意出錯 ----
            "test/garbage" => {
                self.write_raw("this is not json\n").await;
                return Reply::Silent;
            }
            // 空行(有些 server 會多印換行)與 Windows 的 `\r\n`。
            "test/blank-line" => {
                self.write_raw("\n\r\n").await;
                json!("after the blank lines")
            }
            "test/not-utf8" => {
                let mut out = self.out.lock().await;
                let _ = out.write_all(b"\xff\xfe\n").await;
                let _ = out.flush().await;
                return Reply::Silent;
            }
            "test/unknown-id" => {
                self.reply(&json!(424_242), json!("stray")).await;
                json!("real")
            }
            "test/half-line" => {
                self.write_raw(r#"{"jsonrpc":"2.0","id":"#).await;
                std::process::exit(0);
            }
            "test/exit" => {
                eprint!("{}", field(&params, "stderr").as_str().unwrap_or(""));
                let code = field(&params, "code").as_i64().unwrap_or(1);
                std::process::exit(i32::try_from(code).unwrap_or(1));
            }
            // 往 stderr 寫很多:client 沒有持續讀 stderr 的話,pipe 滿了(Linux 約 64 KiB)server 就卡在這裡。
            "test/stderr" => {
                let bytes = field(&params, "bytes").as_u64().unwrap_or(0);
                eprint!("{}", "e".repeat(usize::try_from(bytes).unwrap_or(0)));
                json!("done")
            }
            "test/linger" => {
                self.linger.store(true, Ordering::SeqCst);
                json!("lingering")
            }
            "test/pid" => json!(std::process::id()),
            "test/cancelled" => json!(
                *self
                    .cancelled
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            ),
            _ => return error(-32601, format!("method not found: {method}")),
        };
        Reply::Result(result)
    }

    /// MCP 的方法(第 3 步)。不是 MCP 的方法回 `None`,交給 `test_method`。
    async fn mcp(&self, method: &str, params: &Value) -> Option<Reply> {
        if !matches!(method, "server/discover" | "tools/list" | "tools/call") {
            return None;
        }
        let flags = &self.flags;
        if method == "server/discover" {
            if flags.legacy {
                // 舊版 server 在 `initialize` 之前收到不認得的方法。
                return Some(error(-32601, "Method not found"));
            }
            if flags.silent_discover {
                return Some(Reply::Silent);
            }
        }
        if let Err(reply) = self.check_meta(params) {
            return Some(reply);
        }
        let reply = match method {
            "server/discover" => {
                let capabilities = if flags.no_tools {
                    json!({"resources": {}})
                } else {
                    json!({"tools": {}})
                };
                Reply::Result(json!({
                    "resultType": "complete",
                    "supportedVersions": flags.versions,
                    "capabilities": capabilities,
                    "instructions": "A server for testing MCP clients.",
                    "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "mcp-test-server", "version": "0.1.0"}},
                    "ttlMs": 0,
                    "cacheScope": "private"
                }))
            }
            "tools/list" => self.list_tools(params),
            _ => self.call_tool(params).await,
        };
        Some(reply)
    }

    /// 每個請求都要帶版本與 client 能力(規格 basic §_meta)。少了是 -32602;版本不支援是 -32022。
    fn check_meta(&self, params: &Value) -> Result<(), Reply> {
        let meta = field(params, "_meta");
        let version = field(meta, "io.modelcontextprotocol/protocolVersion").as_str();
        let capabilities = field(meta, "io.modelcontextprotocol/clientCapabilities");
        let (Some(version), true) = (version, capabilities.is_object()) else {
            return Err(error(-32602, "missing required _meta fields"));
        };
        if !self.flags.versions.iter().any(|v| v == version) {
            return Err(Reply::Error(
                -32022,
                "Unsupported protocol version".into(),
                Some(json!({"supported": self.flags.versions, "requested": version})),
            ));
        }
        Ok(())
    }

    fn list_tools(&self, params: &Value) -> Reply {
        const PAGE: usize = 4;
        let tools = tool_definitions(self.flags.bad_tools);
        let page = match field(params, "cursor") {
            Value::Null => 0,
            Value::String(cursor) => match cursor.strip_prefix("page-").map(str::parse::<usize>) {
                Some(Ok(page)) => page,
                _ => return error(-32602, "invalid cursor"),
            },
            _ => return error(-32602, "invalid cursor"),
        };
        let items: Vec<Value> = tools.iter().skip(page * PAGE).take(PAGE).cloned().collect();
        let mut result =
            json!({"resultType": "complete", "tools": items, "ttlMs": 0, "cacheScope": "private"});
        let next = if self.flags.loop_cursor {
            Some("page-1".to_owned())
        } else if (page + 1) * PAGE < tools.len() {
            Some(format!("page-{}", page + 1))
        } else {
            None
        };
        if let (Some(next), Some(object)) = (next, result.as_object_mut()) {
            object.insert("nextCursor".into(), json!(next));
        }
        Reply::Result(result)
    }

    async fn call_tool(&self, params: &Value) -> Reply {
        let name = field(params, "name").as_str().unwrap_or("");
        let args = field(params, "arguments");
        let text =
            |t: String| json!({"resultType": "complete", "content": [{"type": "text", "text": t}]});
        let result = match name {
            "echo" => text(field(args, "text").as_str().unwrap_or("").to_owned()),
            "add" => {
                let a = field(args, "a").as_i64().unwrap_or(0);
                let b = field(args, "b").as_i64().unwrap_or(0);
                text(a.saturating_add(b).to_string())
            }
            "slow" => {
                let ms = field(args, "ms").as_u64().unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                text("done".into())
            }
            "big" => {
                let bytes = field(args, "bytes").as_u64().unwrap_or(0);
                text("x".repeat(usize::try_from(bytes).unwrap_or(0)))
            }
            "fail" => {
                json!({"resultType": "complete", "isError": true, "content": [{"type": "text", "text": "it broke"}]})
            }
            // 各種不是文字的內容,夾著兩段文字。
            "mixed" => json!({"resultType": "complete", "content": [
                {"type": "text", "text": "before"},
                {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"},
                {"type": "audio", "data": "UklGRg==", "mimeType": "audio/wav"},
                {"type": "resource_link", "uri": "file:///notes/a.md", "name": "a.md"},
                {"type": "resource", "resource": {"uri": "file:///notes/b.md", "text": "secret-ish"}},
                {"type": "hologram"},
                {"type": "text", "text": "after"}
            ]}),
            "structured" => {
                json!({"resultType": "complete", "content": [], "structuredContent": {"answer": 42}})
            }
            // 把收到的 `_meta` 原樣交回去,讓測試確認 client 帶了什麼。
            "meta" => text(field(params, "_meta").to_string()),
            // MRTR:要求更多輸入。只帶 requestState(client 沒宣告任何能力,不能帶 inputRequests)。
            "ask" => json!({"resultType": "input_required", "requestState": "opaque"}),
            "mystery" => json!({"resultType": "mystery", "content": []}),
            // 舊版 server 的結果沒有 `resultType`:要當成 complete。
            "plain" => json!({"content": [{"type": "text", "text": "no resultType"}]}),
            // HTTP 模式下,client 要把 `region` 複製到 `Mcp-Param-Region`(由 HTTP handler 檢查)。
            "region" => text(format!(
                "region={}",
                field(args, "region").as_str().unwrap_or("")
            )),
            _ => return error(-32602, format!("Unknown tool: {name}")),
        };
        Reply::Result(result)
    }
}

/// 工具清單。每個都有 `inputSchema`(根的 `type` 是 `object`)。
fn tool_definitions(bad: bool) -> Vec<Value> {
    let no_args = json!({"type": "object", "additionalProperties": false});
    let mut tools = vec![
        json!({"name": "echo", "title": "Echo", "description": "Returns the text it is given.",
               "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}),
        json!({"name": "add", "description": "Adds two integers.",
               "inputSchema": {"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
                               "required": ["a", "b"], "additionalProperties": false}}),
        json!({"name": "slow", "description": "Sleeps, then answers.",
               "inputSchema": {"type": "object", "properties": {"ms": {"type": "integer", "minimum": 0}}}}),
        json!({"name": "big", "description": "Returns a long text.",
               "inputSchema": {"type": "object", "properties": {"bytes": {"type": "integer", "minimum": 0}}}}),
        json!({"name": "fail", "description": "Always fails.", "inputSchema": no_args}),
        json!({"name": "mixed", "description": "Returns text, images and links.", "inputSchema": no_args}),
        json!({"name": "structured", "description": "Returns only structured content.", "inputSchema": no_args}),
        json!({"name": "meta", "description": "Returns the request metadata.", "inputSchema": no_args}),
        json!({"name": "ask", "description": "Asks for more input.", "inputSchema": no_args}),
        json!({"name": "mystery", "description": "Returns an unknown result type.", "inputSchema": no_args}),
        json!({"name": "plain", "inputSchema": no_args}),
        json!({"name": "region", "description": "Answers with the region it was given.",
               "inputSchema": {"type": "object", "properties": {
                   "region": {"type": "string", "x-mcp-header": "Region"}, "query": {"type": "string"}
               }, "required": ["region"]}}),
    ];
    if bad {
        tools.extend([
            json!({"name": 5, "inputSchema": no_args}),
            json!({"name": "no_schema"}),
            json!({"name": "array_schema", "inputSchema": {"type": "array"}}),
            json!("not an object"),
            // `x-mcp-header` 標在小數參數上:規格不允許,整個工具要被排除。
            json!({"name": "bad_header", "inputSchema": {"type": "object", "properties": {
                "n": {"type": "number", "x-mcp-header": "N"}
            }}}),
        ]);
    }
    tools
}

/// 參數裡的一個欄位,沒有就是 `null`。`Value` 的 `[]` 本身也不會 panic,但 clippy 的 `indexing_slicing` 看不出來。
fn field<'a>(params: &'a Value, name: &str) -> &'a Value {
    params.get(name).unwrap_or(&Value::Null)
}

async fn stdio(flags: Flags) -> ExitCode {
    let server = Arc::new(Server {
        out: tokio::sync::Mutex::new(tokio::io::stdout()),
        waiting: Mutex::new(HashMap::new()),
        linger: AtomicBool::new(false),
        flags,
        cancelled: Mutex::new(Vec::new()),
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            eprintln!("not JSON: {line}");
            continue;
        };
        match (message.get("id"), message.get("method")) {
            (Some(id), Some(Value::String(method))) => {
                let (server, id, method) = (Arc::clone(&server), id.clone(), method.clone());
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                tokio::spawn(async move { server.handle(id, &method, params).await });
            }
            // client 對 `test/ask` 的回應。
            (Some(Value::String(id)), None) => {
                let waiting = server
                    .waiting
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(id);
                if let Some(tx) = waiting {
                    let _ = tx.send(message.clone());
                }
            }
            (None, Some(Value::String(method))) if method == "notifications/cancelled" => {
                let id = field(field(&message, "params"), "requestId").clone();
                server
                    .cancelled
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(id);
            }
            // 其他通知、或看不懂的訊息:略過。
            _ => {}
        }
    }
    // stdin 關了:正常的 server 就結束。`test/linger` 之後假裝沒看到,等 client 來 kill。
    if server.linger.load(Ordering::SeqCst) {
        std::future::pending::<()>().await;
    }
    // 結束前花一點時間收尾(像真的 server 存檔)。client 關閉時要等它,不能一關 stdin 就 kill:
    // 立刻結束的話,kill 跟「自己結束」常常分不出先後,測試就看不出 client 有沒有等。
    tokio::time::sleep(Duration::from_millis(100)).await;
    ExitCode::SUCCESS
}

// ---- HTTP 模式(第 4 步)----

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

async fn http(port: u16, flags: Flags) -> ExitCode {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await else {
        eprintln!("cannot bind 127.0.0.1:{port}");
        return ExitCode::from(1);
    };
    let Ok(address) = listener.local_addr() else {
        return ExitCode::from(1);
    };
    // 測試讀這一行拿到 URL。`println!` 之後要 flush:stdout 接到 pipe 時是整塊緩衝的。
    println!("http://{address}/mcp");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let server = Arc::new(Server {
        out: tokio::sync::Mutex::new(tokio::io::stdout()),
        waiting: Mutex::new(HashMap::new()),
        linger: AtomicBool::new(false),
        flags,
        cancelled: Mutex::new(Vec::new()),
    });
    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(handle_http))
        // redirect 的目的地:有人來就記一筆(測試確認 client 沒有跟過來)。
        .route("/elsewhere", axum::routing::any(elsewhere))
        .route("/hits", axum::routing::get(hits))
        .with_state(server);
    match axum::serve(listener, app).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}

static ELSEWHERE_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

async fn elsewhere() -> StatusCode {
    ELSEWHERE_HITS.fetch_add(1, Ordering::SeqCst);
    StatusCode::OK
}

async fn hits() -> String {
    ELSEWHERE_HITS.load(Ordering::SeqCst).to_string()
}

fn rpc_error(
    status: StatusCode,
    id: &Value,
    code: i64,
    message: &str,
    data: Option<Value>,
) -> Response {
    let mut error = json!({"code": code, "message": message});
    if let (Some(data), Some(object)) = (data, error.as_object_mut()) {
        object.insert("data".into(), data);
    }
    let body = json!({"jsonrpc": "2.0", "id": id, "error": error});
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(name)?.to_str().ok()?;
    // `=?base64?…?=` 解回原本的值(規格 Value Encoding)。
    match raw
        .strip_prefix("=?base64?")
        .and_then(|r| r.strip_suffix("?="))
    {
        Some(encoded) => String::from_utf8(decode_base64(encoded)?).ok(),
        None => Some(raw.to_owned()),
    }
}

/// 標準 Base64 解碼。故意不用 client 的編碼器(server 跟 client 共用程式碼的話,錯誤會對稱、測不出來)。
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let text = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &c in text {
        buffer = (buffer << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

async fn handle_http(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let flags = &server.flags;
    if flags.redirect {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "/elsewhere")],
        )
            .into_response();
    }
    if flags.text_response {
        return ([(header::CONTENT_TYPE, "text/plain")], "hello").into_response();
    }
    if let Some(token) = &flags.token {
        let expected = format!("Bearer {token}");
        if headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            != Some(expected.as_str())
        {
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    }
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !(accept.contains("application/json") && accept.contains("text/event-stream")) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "Accept must list application/json and text/event-stream",
        )
            .into_response();
    }
    let Ok(message) = serde_json::from_str::<Value>(&body) else {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            &Value::Null,
            -32700,
            "Parse error",
            None,
        );
    };
    let method = field(&message, "method").as_str().unwrap_or("").to_owned();
    let params = field(&message, "params").clone();
    // 通知:接受就回 202,沒有本文。
    let Some(id) = message.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    // ---- header 跟 body 要一致(規格 Server Validation)----
    let mismatch = |what: &str| {
        rpc_error(
            StatusCode::BAD_REQUEST,
            &id,
            -32020,
            &format!("header mismatch: {what}"),
            None,
        )
    };
    let version = field(
        field(&params, "_meta"),
        "io.modelcontextprotocol/protocolVersion",
    )
    .as_str();
    if header_value(&headers, "mcp-protocol-version").as_deref() != version {
        return mismatch("MCP-Protocol-Version");
    }
    if header_value(&headers, "mcp-method").as_deref() != Some(method.as_str()) {
        return mismatch("Mcp-Method");
    }
    if method == "tools/call" {
        let name = field(&params, "name").as_str();
        if header_value(&headers, "mcp-name").as_deref() != name {
            return mismatch("Mcp-Name");
        }
        if name == Some("region")
            && header_value(&headers, "mcp-param-region").as_deref()
                != field(field(&params, "arguments"), "region").as_str()
        {
            return mismatch("Mcp-Param-Region");
        }
    }
    let reply = match server.mcp(&method, &params).await {
        Some(reply) => reply,
        None => return rpc_error(StatusCode::NOT_FOUND, &id, -32601, "Method not found", None),
    };
    let response = match reply {
        Reply::Result(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Reply::Error(code, message, data) => {
            // 規格:版本不支援、少了必填的 _meta → 400;其他錯誤(例如不認得的工具)照常回 200 + 錯誤。
            let status = if matches!(code, -32022 | -32602) && method != "tools/call" {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            };
            return rpc_error(status, &id, code, &message, data);
        }
        Reply::Silent => {
            std::future::pending::<()>().await;
            return StatusCode::NO_CONTENT.into_response();
        }
    };
    if !flags.sse {
        // 帶 charset 參數:client 要看 media type 本身,不能整串比對。
        return (
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            response.to_string(),
        )
            .into_response();
    }
    // SSE:先送一個註解、一個空資料的事件(舊版規格建議的「預備」事件)、一個通知,client 都要略過;最後是回應。
    let mut events = String::new();
    events.push_str(": keep-alive comment\n\n");
    events.push_str("id: 0\ndata:\n\n");
    let progress =
        json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progress": 1}});
    events.push_str(&format!("event: message\ndata: {progress}\n\n"));
    if flags.sse_request {
        let request = json!({"jsonrpc": "2.0", "id": "s-1", "method": "sampling/createMessage", "params": {}});
        events.push_str(&format!("data: {request}\n\n"));
    }
    if !flags.sse_truncate {
        // 把 JSON 拆成好幾行 `data:`:SSE 會用換行接回來,JSON 允許中間有換行。
        let text = serde_json::to_string_pretty(&response).unwrap_or_default();
        for line in text.lines() {
            events.push_str(&format!("data: {line}\n"));
        }
        events.push('\n');
    }
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(events))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
