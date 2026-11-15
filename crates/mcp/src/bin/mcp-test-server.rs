//! 測試用的 MCP server(design-part2 §7、§9-4,D-051)。
//!
//!     mcp-test-server --stdio
//!
//! 整合測試用 `env!("CARGO_BIN_EXE_mcp-test-server")` 找到它。CI 不必裝 Node 或 Python,而且可以照劇本故意出錯
//! (真的 server 不會配合)。
//!
//! 刻意**不用** `mcp::jsonrpc` 解析與編碼,直接操作 `serde_json::Value`:server 跟 client 共用同一個 parser 的話,
//! parser 的 bug 兩邊對稱,測試就看不出來。
//!
//! 這一步(第 2 步)只有測試傳輸用的 `test/*` 方法;MCP 本身的 `initialize`、`tools/*` 在第 3 步加上。

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};
use tokio::sync::oneshot;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode] if mode == "--stdio" => stdio().await,
        _ => {
            eprintln!("usage: mcp-test-server --stdio");
            ExitCode::from(2)
        }
    }
}

struct Server {
    /// 一則訊息要一次寫完一整行:多個請求並行處理,共用 stdout 時要排隊。
    /// 這裡用 tokio 的 `Mutex`:寫入要 `.await`,std 的鎖不能跨 `.await` 拿著。
    out: tokio::sync::Mutex<Stdout>,
    /// server 送給 client 的請求(`test/ask`),等 client 回應。key 是我們配的字串 id。
    waiting: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// `test/linger` 之後,stdin 關掉也不結束(測試 client 關閉時會不會 kill)。
    linger: AtomicBool,
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

    /// 處理一個請求。每個請求在自己的 task 裡跑,所以 `test/sleep` 不會擋住其他請求(測試回應的順序可以跟請求不同)。
    async fn handle(&self, id: Value, method: &str, params: Value) {
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
                return;
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
                return;
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
            _ => {
                self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {method}")}}))
                    .await;
                return;
            }
        };
        self.reply(&id, result).await;
    }
}

/// 參數裡的一個欄位,沒有就是 `null`。`Value` 的 `[]` 本身也不會 panic,但 clippy 的 `indexing_slicing` 看不出來。
fn field<'a>(params: &'a Value, name: &str) -> &'a Value {
    params.get(name).unwrap_or(&Value::Null)
}

async fn stdio() -> ExitCode {
    let server = Arc::new(Server {
        out: tokio::sync::Mutex::new(tokio::io::stdout()),
        waiting: Mutex::new(HashMap::new()),
        linger: AtomicBool::new(false),
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
            // 通知、或看不懂的訊息:略過。
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
