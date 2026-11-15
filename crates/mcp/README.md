# mcp

從零寫的 MCP(Model Context Protocol)client:讓 agent 用別人寫好的工具。只做工具(`tools/list`、`tools/call`),
協定版本 **2026-07-28**,stdio 與 Streamable HTTP 兩種傳輸都支援。設計與取捨見
[`design-part2.md`](../../docs/modules/003-agent-runtime/design-part2.md)(D-047 ~ D-052)。

```
McpClient ── server/discover ──→ 是新版 server?支援 2026-07-28?有工具?
          ── tools/list ───────→ 翻完每一頁(有上限),格式不對的工具個別拒絕
          ── tools/call ───────→ content[] 變成一段文字、isError
              每個請求的 params._meta 自己帶:協定版本、client 能力(空的)、client 名稱(沒有 initialize、沒有 session)
    │
    └─ Box<dyn Transport>
         ├─ StdioTransport  子行程,一行一則 JSON-RPC;清空環境變數、kill_on_drop、stderr 只留最後 4 KiB
         └─ HttpTransport   一個請求一個 POST;回應是 JSON 或 SSE;不跟隨 redirect、只收 https(本機例外)
```

這個 crate 不知道 agent 存在;把 MCP 工具接到 agent 的 `ErasedTool`(白名單、參數驗證)在 [`crates/agent`](../agent/README.md#mcp-server)。

## 快速開始

不需要任何外部程式:repo 裡有一個照規格寫的測試 server(`mcp-test-server`)。

```bash
cargo build -p mcp --bin mcp-test-server
cargo run -p mcp --example list_tools -- target/debug/mcp-test-server --stdio
cargo run -p mcp --example list_tools -- --call add '{"a": 2, "b": 3}' target/debug/mcp-test-server --stdio
```

接真的 server(要 Node):

```bash
cargo run -p mcp --example list_tools -- npx -y @modelcontextprotocol/server-everything@2.3.0 stdio
```

## 當 library 用

```rust
use std::collections::BTreeMap;
use mcp::transport::{StdioConfig, StdioLimits, StdioTransport};
use mcp::{ClientOptions, McpClient};

let config = StdioConfig {
    command: "notes-mcp".into(),
    args: vec!["--root".into(), "/Users/me/notes".into()],
    // 子行程的「全部」環境變數:要讓它找得到其他程式,自己把 PATH 放進來(library 不讀環境變數)。
    env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
};
let transport = StdioTransport::spawn(&config, StdioLimits::default())?;
let client = McpClient::connect(Box::new(transport), ClientOptions::default()).await?;
let tools = client.list_tools().await?;        // tools.tools、tools.rejected
let result = client.call_tool("search", arguments).await?;   // result.text、result.is_error
client.close().await;                          // 關 stdin → 等 2 秒 → kill
```

## 安全重點

server 送來的一切都當不可信輸入:

| 項目 | 怎麼處理 |
|---|---|
| 訊息格式 | JSON-RPC 逐條規則檢查(`result` 與 `error` 恰好一個、id 是整數或字串……);stdio 上違反協定就關掉連線 |
| 大小 | 一則訊息 4 MiB;SSE 每個事件 4 MiB、整個串流 16 MiB;`tools/list` 最多 100 頁、1000 個工具,同一個 cursor 出現兩次就停 |
| 工具定義 | 名稱 1~128 個 `A-Z a-z 0-9 _ - .`;`inputSchema` 根是 object;`x-mcp-header` 照規格檢查;不合格的個別拒絕 |
| 要印出來的字串 | 版本、被拒絕的工具名稱、錯誤本文、stderr:控制字元換掉、限制長度 |
| 子行程 | 只拿到設定的環境變數(API key 不會漏給第三方程式);agent 不管怎麼結束都會收掉它 |
| HTTP | 不跟隨 redirect(token 不會被帶到別的主機);錯誤訊息不含 URL;URL 不能含帳號密碼;token 是 `SecretString` |
| server 的反問 | 不宣告任何 client 能力,所以 server 不會要求 sampling 之類;舊版 server 在 stdio 送來請求時回 `-32601` |

## 測試

| 層 | 檔案 | 內容 |
|---|---|---|
| 單元 | `src/**/tests.rs` | JSON-RPC 的每條規則、一行一行讀的上限、stderr 尾巴、`x-mcp-header` 與 base64、結果轉換、名稱規則 |
| 整合 | `tests/stdio.rs` | stdio 傳輸對測試 server:id 對應、並行、取消、環境變數、上限、server 死掉、關閉 |
| 整合 | `tests/client.rs` | `McpClient` 的每個情境,**stdio 與 HTTP 各跑一次** |
| 整合 | `tests/http.rs` | HTTP 專屬:token、redirect、SSE、上限、狀態碼 |
| Live | `tests/live.rs`(`#[ignore]`) | 對官方參考 server 跑一次,見下面 |

測試 server(`src/bin/mcp-test-server.rs`)刻意不用這個 crate 的 parser,直接操作 JSON:共用 parser 的話,parser 的 bug 兩邊對稱、測不出來。
它可以照旗標扮演各種不配合的 server(舊版、不支援的版本、沒有工具、不回應、壞掉的工具、繞圈的 cursor、SSE 裡夾請求……)。

### 對官方參考 server 跑(手動)

```bash
# stdio:測試自己用 npx 啟動 server
cargo test -p mcp --test live -- --ignored live_stdio

# HTTP:先在另一個終端機啟動 server
PORT=3001 npx -y @modelcontextprotocol/server-everything@2.3.0 streamableHttp
cargo test -p mcp --test live -- --ignored live_http
```

> 2026-11-15:在開發用的雲端環境裡,執行從 npm 下載的程式被權限設定擋下,所以這兩個測試**還沒有實際跑過**。
> 已經讀過參考 server 2.3.0 的原始碼(工具名稱與 schema,`get-header-params` 用 `x-mcp-header`),測試照它寫。

## 原始碼導覽

| 檔案 | 內容 |
|---|---|
| `src/jsonrpc.rs` | JSON-RPC 2.0 的訊息:編碼、逐條規則解析 |
| `src/transport/mod.rs` | `Transport` trait(`BoxFuture`、`close(self: Box<Self>)`) |
| `src/transport/stdio.rs` | 子行程、讀 stdout 的背景 task、依 id 交給等待中的請求(`HashMap<id, oneshot::Sender>`)、取消、stderr、關閉 |
| `src/transport/http.rs` | POST、header、JSON 或 SSE 的回應、錯誤狀態碼 |
| `src/headers.rs` | `x-mcp-header` 的規則、header 值的編碼、base64 |
| `src/client.rs` | `McpClient`:`server/discover` 探測、`tools/list`、`tools/call`、`_meta`、逾時 |
| `src/error.rs` | `McpError` |
| `src/bin/mcp-test-server.rs` | 測試用的 server(stdio 與 HTTP) |
| `examples/list_tools.rs` | 連上 server、列出工具、呼叫一個 |
