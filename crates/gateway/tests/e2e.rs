//! E2E:真的執行 gateway 程式,上游的 Claude 換成 wiremock(design-part2.md §21 第 18 步)。
//!
//! ```text
//! 測試(HTTP client)──真的 TCP──→ gateway 子行程 ──真的 TCP──→ wiremock(假 Claude)
//! ```
//!
//! router 測試(tower `oneshot`)不開 port、不經過 `main`;這裡補上它們碰不到的部分:
//! `main` 讀設定與環境變數、`anthropic.base_url` 真的被使用、port 0、真的 socket 上的 Host 檢查、
//! SIGTERM 之後的優雅關機與紀錄寫完。
//!
//! Java 對照:Spring Boot 的 `@SpringBootTest(webEnvironment = RANDOM_PORT)`,
//! 但這裡更徹底:gateway 是另一個行程,跟正式啟動的方式完全一樣。

// 整個檔案都是測試,輔助函式也一樣:失敗就該 panic。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sk-ant-e2e-fake-key";
const TOKEN: &str = "e2e-token";
/// 等 gateway 印出某一行、等它結束,最多等這麼久。錯的實作要讓測試「失敗」,而不是「卡住」。
const PATIENCE: Duration = Duration::from_secs(10);

/// 一個正在執行的 gateway 子行程。
struct Gateway {
    child: Child,
    /// `http://127.0.0.1:<port>`:port 是作業系統挑的(設定檔寫 0)。
    base: String,
    /// stderr 一行一行送過來。
    stderr: mpsc::UnboundedReceiver<String>,
    /// 到目前為止讀到的 stderr,失敗時印出來方便除錯。
    log: Vec<String>,
    /// 設定檔與紀錄目錄都在這裡;`TempDir` 被丟掉時整個刪除,所以要跟著 Gateway 活著。
    dir: TempDir,
    http: reqwest::Client,
}

impl Gateway {
    /// 寫一份設定檔、啟動 gateway,等它印出 `listening on` 那一行。
    async fn start(upstream: &MockServer) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = format!(
            r#"
[server]
bind = "127.0.0.1:0"

[aliases]
default = "claude-sonnet-5-5"

[anthropic]
base_url = "{}"

[record]
dir = "records"

[retry]
max_attempts = 3
first_delay_ms = 1
max_delay_ms = 20
"#,
            upstream.uri()
        );
        std::fs::write(dir.path().join("gateway.toml"), config).unwrap();

        // `env!("CARGO_BIN_EXE_gateway")`:cargo 跑整合測試前會先編好 binary,並把路徑放在這個編譯期變數裡。
        let mut child = Command::new(env!("CARGO_BIN_EXE_gateway"))
            .arg("--config")
            .arg(dir.path().join("gateway.toml"))
            // 工作目錄放在暫存目錄:gateway 會往上找 .env,不能讀到 repo 裡真的那一份。
            .current_dir(dir.path())
            // 命令列給的環境變數優先於 .env(dotenvy 不覆寫已經存在的變數)。key 是假的。
            .env("ANTHROPIC_API_KEY", KEY)
            .env("GATEWAY_TOKEN", TOKEN)
            .env("RUST_LOG", "info")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // 測試失敗(panic)時,Child 被丟掉 → 子行程被殺掉,不會留下孤兒行程佔著 port。
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        // stderr 一定要持續讀:pipe 的緩衝區滿了,子行程寫 log 時就會卡住。
        let (tx, stderr) = mpsc::unbounded_channel();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let mut gateway = Self {
            child,
            base: String::new(),
            stderr,
            log: vec![],
            dir,
            http: reqwest::Client::new(),
        };
        let line = gateway.wait_for_line("listening on http://").await;
        // `listening on http://127.0.0.1:54321 (Ctrl+C to stop)` → `127.0.0.1:54321`
        let addr = line
            .split("http://")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap();
        gateway.base = format!("http://{addr}");
        gateway
    }

    /// 讀 stderr,直到出現包含 `needle` 的一行。
    async fn wait_for_line(&mut self, needle: &str) -> String {
        let found = tokio::time::timeout(PATIENCE, async {
            while let Some(line) = self.stderr.recv().await {
                self.log.push(line.clone());
                if line.contains(needle) {
                    return Some(line);
                }
            }
            None
        })
        .await;
        match found {
            Ok(Some(line)) => line,
            _ => panic!(
                "gateway never printed `{needle}`; stderr:\n{}",
                self.log.join("\n")
            ),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// 帶 token 的 POST。
    async fn post(&self, path: &str, body: &Value) -> reqwest::Response {
        self.http
            .post(self.url(path))
            .bearer_auth(TOKEN)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap()
    }

    /// 送 SIGTERM(用 `kill` 指令,跟 Docker / systemd 停止服務時送的信號一樣)。
    // 不用 tokio 的 `Child::kill()`:那是 SIGKILL,行程沒有機會收尾,測不到優雅關機。
    fn send_sigterm(&self) {
        let pid = self.child.id().unwrap();
        let sent = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
    }

    /// 送 SIGTERM,等它結束,回傳 exit code。
    async fn terminate(&mut self) -> Option<i32> {
        self.send_sigterm();
        self.wait_for_exit().await
    }

    async fn wait_for_exit(&mut self) -> Option<i32> {
        match tokio::time::timeout(PATIENCE, self.child.wait()).await {
            Ok(status) => status.unwrap().code(),
            Err(_) => panic!("gateway did not exit; stderr:\n{}", self.log.join("\n")),
        }
    }

    /// 紀錄目錄裡全部的紀錄(每行一筆 JSON)。
    fn records(&self) -> Vec<Value> {
        let dir: PathBuf = self.dir.path().join("records");
        let mut records = vec![];
        for entry in std::fs::read_dir(&dir).unwrap() {
            let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
            records.extend(
                text.lines()
                    .map(|l| serde_json::from_str::<Value>(l).unwrap()),
            );
        }
        records
    }
}

/// 呼叫者送來的請求:跟 README 裡的 curl 範例一樣,只寫必要的欄位。
fn chat_request() -> Value {
    json!({
        "model": "default",
        "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }],
        "max_tokens": 64
    })
}

/// 上游(Anthropic)的 SSE 回應:回答「Hello」。
fn hello() -> ResponseTemplate {
    let events = [
        json!({ "type": "message_start", "message": { "id": "msg_1", "model": "claude-sonnet-5-5", "usage": { "input_tokens": 9, "output_tokens": 1 } } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hel" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "lo" } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 2 } }),
        json!({ "type": "message_stop" }),
    ];
    let body: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    // `set_body_string` 會把 content-type 蓋成 text/plain,要用 `set_body_raw` 指定。
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// 上游的 `/v1/messages`:一定要帶著 gateway 的 API key,而且模型是別名換過之後的名字。
fn messages_endpoint() -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", KEY))
        .and(body_partial_json(json!({ "model": "claude-sonnet-5-5" })))
}

async fn upstream_calls(upstream: &MockServer) -> usize {
    upstream.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn chat_goes_through_the_whole_chain_to_the_configured_upstream() {
    let upstream = MockServer::start().await;
    messages_endpoint()
        .respond_with(hello())
        .expect(1)
        .mount(&upstream)
        .await;
    let mut gateway = Gateway::start(&upstream).await;

    let reply = gateway.post("/v1/chat", &chat_request()).await;

    // 200 + Hello:請求真的打到了設定檔裡的 base_url(wiremock),別名換成了真的模型名稱,帶著 API key。
    assert_eq!(reply.status(), 200);
    let body: Value = serde_json::from_str(&reply.text().await.unwrap()).unwrap();
    assert_eq!(body["model"], "claude-sonnet-5-5");
    assert_eq!(
        body["content"],
        json!([{ "type": "text", "text": "Hello" }])
    );

    assert_eq!(gateway.terminate().await, Some(0));
    // 優雅關機會等紀錄寫完:結束之後檔案裡一定有這一筆。
    let records = gateway.records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["requested_model"], "default");
    assert_eq!(records[0]["served_model"], "claude-sonnet-5-5");
    assert_eq!(records[0]["outcome"], "ok");
}

#[tokio::test]
async fn the_startup_summary_says_where_the_records_go() {
    // 紀錄預設寫在專案外,啟動時一定要印出來,讓人找得到(D-021)。
    // (cargo-mutants 找到的缺口:把 print_summary 換成什麼都不做,原本沒有測試失敗。)
    let upstream = MockServer::start().await;
    let gateway = Gateway::start(&upstream).await;

    // 相對路徑 `records` 以設定檔所在目錄為準(D-025)。
    let expected = format!(
        "  records  {}",
        gateway.dir.path().join("records").display()
    );
    assert!(
        gateway.log.contains(&expected),
        "missing `{expected}` in:\n{}",
        gateway.log.join("\n")
    );
    assert!(gateway.log.contains(&format!(
        "  bind     {} (local only)",
        gateway.base.trim_start_matches("http://")
    )));
}

#[tokio::test]
async fn stream_relays_sse_events_over_a_real_socket() {
    let upstream = MockServer::start().await;
    messages_endpoint()
        .respond_with(hello())
        .mount(&upstream)
        .await;
    let gateway = Gateway::start(&upstream).await;

    let reply = gateway.post("/v1/chat/stream", &chat_request()).await;

    assert_eq!(reply.status(), 200);
    assert!(
        reply.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let body = reply.text().await.unwrap();
    let names: Vec<&str> = body
        .lines()
        .filter_map(|l| l.strip_prefix("event: "))
        .collect();
    assert_eq!(
        names,
        [
            "message_start",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "finish"
        ]
    );
    assert!(
        body.contains(r#"data: {"type":"text_delta","index":0,"text":"Hel"}"#),
        "{body}"
    );
}

#[tokio::test]
async fn the_guards_protect_the_real_socket() {
    let upstream = MockServer::start().await;
    let gateway = Gateway::start(&upstream).await;
    let get = |path: &str| gateway.http.get(gateway.url(path));

    // `/health` 不需要 token(服務檢查用,D-025)。
    let health = get("/health").send().await.unwrap();
    assert_eq!(health.status(), 200);

    // 沒有 token → 401。
    let anonymous = get("/v1/models").send().await.unwrap();
    assert_eq!(anonymous.status(), 401);
    assert_eq!(anonymous.headers()["www-authenticate"], "Bearer");

    // 有 token、Host 正確(reqwest 自動送 `127.0.0.1:<實際的 port>`)→ 200。
    // port 0 的修正就是讓這一條成立:允許清單裡要是實際的 port,不是 0。
    let models = get("/v1/models").bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(models.status(), 200);
    let models: Value = serde_json::from_str(&models.text().await.unwrap()).unwrap();
    assert_eq!(
        models["models"],
        json!([{ "alias": "default", "model": "claude-sonnet-5-5" }])
    );

    // 有 token、但 Host 是別人的(DNS rebinding)→ 403。
    let rebinding = get("/v1/models")
        .bearer_auth(TOKEN)
        .header("host", "evil.example:80")
        .send()
        .await
        .unwrap();
    assert_eq!(rebinding.status(), 403);

    // 擋下的請求都沒有碰到上游。
    assert_eq!(upstream_calls(&upstream).await, 0);
}

#[tokio::test]
async fn an_overloaded_upstream_is_retried_and_the_caller_sees_one_answer() {
    let upstream = MockServer::start().await;
    // 第一次 529(Anthropic 過載),之後正常。`up_to_n_times(1)`:這個 mock 只回應一次,
    // 用完之後換優先順序較低的下一個 mock 接手。
    messages_endpoint()
        .respond_with(ResponseTemplate::new(529).set_body_raw(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            "application/json",
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&upstream)
        .await;
    messages_endpoint()
        .respond_with(hello())
        .with_priority(2)
        .mount(&upstream)
        .await;
    let mut gateway = Gateway::start(&upstream).await;

    let reply = gateway.post("/v1/chat", &chat_request()).await;

    assert_eq!(reply.status(), 200);
    assert_eq!(upstream_calls(&upstream).await, 2);
    // Record 在 Retry 外面(D-020):重試是一次呼叫的內部細節,只記一筆。
    assert_eq!(gateway.terminate().await, Some(0));
    let records = gateway.records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["outcome"], "ok");
}

#[tokio::test]
async fn a_rejected_api_key_is_a_gateway_problem_not_the_callers() {
    let upstream = MockServer::start().await;
    messages_endpoint()
        .respond_with(ResponseTemplate::new(401).set_body_raw(
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
            "application/json",
        ))
        .mount(&upstream)
        .await;
    let gateway = Gateway::start(&upstream).await;

    let reply = gateway.post("/v1/chat", &chat_request()).await;

    // 呼叫者的 token 是對的;錯的是 gateway 的 API key → 502,不是 401(design-part2.md §17)。
    assert_eq!(reply.status(), 502);
    let body: Value = serde_json::from_str(&reply.text().await.unwrap()).unwrap();
    assert_eq!(body["error"]["kind"], "upstream_auth");
    assert_eq!(body["error"]["retryable"], false);
    // 不能重試的錯誤:上游只被打一次。
    assert_eq!(upstream_calls(&upstream).await, 1);
}

#[tokio::test]
async fn sigterm_lets_the_request_in_flight_finish_then_exits_cleanly() {
    let upstream = MockServer::start().await;
    // 上游要 1 秒才回答:SIGTERM 送到時,這個請求一定還在路上。
    messages_endpoint()
        .respond_with(hello().set_delay(Duration::from_secs(1)))
        .mount(&upstream)
        .await;
    let mut gateway = Gateway::start(&upstream).await;

    // 在背景送出請求。reqwest::Client 內部是 Arc,clone 只是共用同一個連線池。
    let http = gateway.http.clone();
    let url = gateway.url("/v1/chat");
    let in_flight = tokio::spawn(async move {
        http.post(url)
            .bearer_auth(TOKEN)
            .header("content-type", "application/json")
            .body(chat_request().to_string())
            .send()
            .await
    });
    // 等到上游確實收到請求,才送 SIGTERM。
    wait_until(|| async { upstream_calls(&upstream).await == 1 }).await;

    gateway.send_sigterm();
    gateway.wait_for_line("shutdown signal received").await;

    // 停止信號之後,進行中的請求照樣做完,呼叫者拿到完整的回答。
    let reply = in_flight.await.unwrap().unwrap();
    assert_eq!(reply.status(), 200);
    assert!(reply.text().await.unwrap().contains("Hello"));

    // 然後才結束:exit code 0,紀錄寫完。
    gateway.wait_for_line("gateway: stopped").await;
    assert_eq!(gateway.wait_for_exit().await, Some(0));
    let records = gateway.records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["outcome"], "ok");
}

#[tokio::test]
async fn a_bad_config_stops_startup_with_every_problem_listed() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(
        &config,
        "[aliases]\n[retry]\nmax_attempts = 0\n[anthropic]\nbase_url = \"http://api.example.com\"\n",
    )
    .unwrap();

    let output = run_to_completion(&config, dir.path()).await;

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    // 三個問題一次列出(D-024)。
    for problem in [
        "[aliases] must define at least one alias",
        "anthropic.base_url `http://api.example.com` must use https",
        "retry.max_attempts must be at least 1",
    ] {
        assert!(
            stderr.contains(problem),
            "missing `{problem}` in:\n{stderr}"
        );
    }
}

/// 執行 gateway 直到它自己結束(用在「應該啟動失敗」的情況)。
async fn run_to_completion(config: &Path, dir: &Path) -> std::process::Output {
    let run = Command::new(env!("CARGO_BIN_EXE_gateway"))
        .arg("--config")
        .arg(config)
        .current_dir(dir)
        .env("ANTHROPIC_API_KEY", KEY)
        .env("GATEWAY_TOKEN", TOKEN)
        .env("NO_COLOR", "1")
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(PATIENCE, run)
        .await
        .expect("gateway should exit on a bad config")
        .unwrap()
}

/// 每 10 ms 檢查一次條件,最多等 `PATIENCE`。
async fn wait_until<F, Fut>(mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let waited = tokio::time::timeout(PATIENCE, async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "condition never became true");
}
