//! E2E:真的執行 agent binary,上游的 Claude 是 wiremock(design §11 第 5 步)。
//!
//! 驗證 composition root 的組裝:讀設定、讀 API key、Record 寫進呼叫紀錄、軌跡寫進私有目錄、
//! 多輪的工具呼叫(先 tool_use、再 end_turn)、護欄、結束碼。

// 整個檔案都是測試:helper 函式不在 `#[test]` 裡,clippy.toml 的「測試裡放寬」管不到它們,在這裡一次放寬(同 eval 的 E2E)。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const KEY: &str = "sk-ant-e2e-fake-key";
const PATIENCE: Duration = Duration::from_secs(60);

/// 假的 Claude:對話裡還沒有工具結果,就要求用 calculator 算 6 * 7;有了就回答。
/// `looping` 時永遠要求呼叫工具(每次一個新的 id),用來測護欄。
struct FakeClaude {
    looping: bool,
    /// 要求呼叫不需要參數的 `list_dir`:串流裡沒有任何 `input_json_delta`(Anthropic 對空參數就是這樣送)。
    no_args: bool,
    calls: AtomicUsize,
}

impl Respond for FakeClaude {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let has_result = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .any(|block| block["type"] == "tool_result");
        if has_result && !self.looping {
            text_sse("6 乘 7 等於 42。")
        } else if self.no_args {
            sse(&[
                start(),
                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": format!("toolu_{n}"), "name": "list_dir", "input": {} } }),
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 20 } }),
                json!({ "type": "message_stop" }),
            ])
        } else {
            tool_sse(
                &format!("toolu_{n}"),
                "calculator",
                r#"{"expression": "6 * 7"}"#,
            )
        }
    }
}

fn sse(events: &[Value]) -> ResponseTemplate {
    let body: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn start() -> Value {
    json!({ "type": "message_start", "message": { "id": "msg_1", "model": "claude-haiku-5-5", "usage": { "input_tokens": 1000, "output_tokens": 1 } } })
}

fn text_sse(text: &str) -> ResponseTemplate {
    sse(&[
        start(),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 30 } }),
        json!({ "type": "message_stop" }),
    ])
}

fn tool_sse(id: &str, name: &str, args: &str) -> ResponseTemplate {
    sse(&[
        start(),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": args } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 20 } }),
        json!({ "type": "message_stop" }),
    ])
}

/// 測試用的工作目錄:設定檔、資料目錄、紀錄目錄、給檔案工具讀的 notes/。
struct Lab {
    dir: TempDir,
    upstream: MockServer,
}

impl Lab {
    async fn start(looping: bool, extra_config: &str) -> Lab {
        Lab::start_with(looping, false, extra_config).await
    }

    async fn start_with(looping: bool, no_args: bool, extra_config: &str) -> Lab {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            // 一定要帶 API key:證明 main 讀了環境變數、組裝了 provider。
            .and(header("x-api-key", KEY))
            .respond_with(FakeClaude {
                looping,
                no_args,
                calls: AtomicUsize::new(0),
            })
            .mount(&upstream)
            .await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("notes")).unwrap();
        std::fs::write(dir.path().join("notes/todo.md"), "- 買牛奶").unwrap();
        let config = format!(
            r#"
[aliases]
default = "claude-sonnet-5-5"
cheap = "claude-haiku-5-5"
[anthropic]
base_url = "{}"
[paths]
data_dir = "data"
records = "records"
[prices."claude-haiku-5-5"]
input = 0.10
output = 0.50
{extra_config}
"#,
            upstream.uri()
        );
        std::fs::write(dir.path().join("agent.toml"), config).unwrap();
        Lab { dir, upstream }
    }

    /// 執行 agent。工作目錄是暫存目錄:不會讀到 repo 裡的 `.env`。
    async fn agent(&self, args: &[&str], key: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent"));
        cmd.args(args)
            .current_dir(self.dir.path())
            .env_remove("ANTHROPIC_API_KEY")
            .env("RUST_BACKTRACE", "0");
        if let Some(key) = key {
            cmd.env("ANTHROPIC_API_KEY", key);
        }
        tokio::time::timeout(PATIENCE, cmd.output())
            .await
            .expect("agent did not finish in time")
            .unwrap()
    }

    async fn requests(&self) -> Vec<Value> {
        self.upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    fn files(&self, dir: &str) -> Vec<PathBuf> {
        match std::fs::read_dir(self.dir.path().join(dir)) {
            Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn a_task_is_answered_with_a_tool_and_everything_is_recorded() {
    let lab = Lab::start(false, "").await;
    let out = lab
        .agent(&["run", "6 乘 7 是多少?", "--model", "cheap"], Some(KEY))
        .await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    // 回答印到 stdout,過程印到 stderr。
    assert_eq!(stdout, "6 乘 7 等於 42。\n");
    assert!(
        stderr.contains(r#"[1] calculator {"expression": "6 * 7"} → 42"#),
        "{stderr}"
    );
    assert!(stderr.contains("turns 2/10 · tool calls 1"), "{stderr}");
    // 兩輪各 1000 input + 20/30 output 個 token,Haiku 價格:0.002 * 0.10 + 0.00005 * 0.50。
    assert!(stderr.contains("cost $0.0002"), "{stderr}");

    // 第二次請求帶著工具結果;第一次請求只有預設的兩個工具(沒給 --root)。
    let requests = lab.requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], "claude-haiku-5-5");
    let tools: Vec<&str> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["calculator", "current_time"]);
    let result = &requests[1]["messages"][2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["content"], "42");

    // 呼叫紀錄:一次模型呼叫一筆(D-020),檔案 600。
    let records = lab.files("records");
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(lines(&records[0]).len(), 2);
    assert_eq!(mode(&records[0]), 0o600);

    // 軌跡:start、兩個 step、finish;目錄 700、檔案 600。
    let runs = lab.dir.path().join("data/runs");
    assert_eq!(mode(&runs), 0o700);
    let trajectories = lab.files("data/runs");
    assert_eq!(trajectories.len(), 1);
    assert_eq!(mode(&trajectories[0]), 0o600);
    assert!(stderr.contains(&format!("trajectory: {}", trajectories[0].display())));
    let events = lines(&trajectories[0]);
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["start", "step", "step", "finish"]);
    assert_eq!(events[0]["model"], "cheap");
    assert_eq!(events[0]["task"], "6 乘 7 是多少?");
    assert_eq!(events[1]["step"]["tool_calls"][0]["result"], "42");
    assert_eq!(
        events[3]["outcome"],
        json!({"kind": "answered", "text": "6 乘 7 等於 42。"})
    );
}

#[tokio::test]
async fn no_record_keeps_the_calls_out_of_the_records() {
    let lab = Lab::start(false, "").await;
    let out = lab
        .agent(
            &["run", "6 乘 7?", "--model", "cheap", "--no-record"],
            Some(KEY),
        )
        .await;
    assert!(out.status.success(), "{:?}", text(&out));
    assert!(lab.files("records").is_empty());
    // 軌跡照樣寫。
    assert_eq!(lab.files("data/runs").len(), 1);
}

#[tokio::test]
async fn the_file_tools_are_offered_only_with_a_root() {
    let lab = Lab::start(false, "").await;
    let out = lab
        .agent(
            &["run", "6 乘 7?", "--model", "cheap", "--root", "notes"],
            Some(KEY),
        )
        .await;
    assert!(out.status.success(), "{:?}", text(&out));
    let tools: Vec<String> = lab.requests().await[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        tools,
        ["calculator", "current_time", "read_file", "list_dir"]
    );

    let out = lab
        .agent(&["run", "?", "--root", "missing"], Some(KEY))
        .await;
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out)
            .1
            .contains("--root missing must be an existing directory")
    );
}

#[tokio::test]
async fn a_model_that_never_answers_is_stopped_by_the_turn_limit() {
    let lab = Lab::start(true, "[limits]\nmax_turns = 3\n").await;
    let out = lab
        .agent(&["run", "?", "--model", "cheap"], Some(KEY))
        .await;
    let (stdout, stderr) = text(&out);
    // 執行了但沒有回答:結束碼 1,stdout 是空的。
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("stopped: reached the turn limit (3)"),
        "{stderr}"
    );
    assert_eq!(lab.requests().await.len(), 3);
    // `--max-turns` 覆寫設定檔。
    let out = lab
        .agent(
            &["run", "?", "--model", "cheap", "--max-turns", "1"],
            Some(KEY),
        )
        .await;
    assert!(text(&out).1.contains("stopped: reached the turn limit (1)"));
    assert_eq!(lab.requests().await.len(), 4);
}

#[tokio::test]
async fn setup_errors_stop_before_any_model_call() {
    let lab = Lab::start(false, "").await;
    // 沒有 API key。
    let out = lab.agent(&["run", "?"], None).await;
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out).1.contains("the API key is needed"),
        "{:?}",
        text(&out)
    );
    // 不認得的別名。
    let out = lab.agent(&["run", "?", "--model", "huge"], Some(KEY)).await;
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out)
            .1
            .contains("unknown model alias `huge`; [aliases] in the config has: cheap, default")
    );
    // 設定檔錯誤:一次列出全部。
    let bad = Lab::start(false, "[limits]\nmax_turns = 0\nmax_parallel = 0\n").await;
    let out = bad.agent(&["run", "?"], Some(KEY)).await;
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr.contains("limits.max_turns must be at least 1"),
        "{stderr}"
    );
    assert!(
        stderr.contains("limits.max_parallel must be at least 1"),
        "{stderr}"
    );
    assert!(lab.requests().await.is_empty());
    assert!(bad.requests().await.is_empty());
}

#[tokio::test]
async fn a_tool_called_without_arguments_goes_back_as_an_empty_object() {
    // 003 第 7 步真的跑才發現:沒有參數的工具呼叫,串流裡沒有 `input_json_delta`,
    // 參數組成空字串,下一輪送回 Anthropic 時被當成不合法的 JSON 拒絕。
    let lab = Lab::start_with(false, true, "").await;
    let out = lab
        .agent(
            &[
                "run",
                "notes 裡有什麼?",
                "--model",
                "cheap",
                "--root",
                "notes",
            ],
            Some(KEY),
        )
        .await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("[1] list_dir {} → todo.md"), "{stderr}");
    let requests = lab.requests().await;
    assert_eq!(requests.len(), 2);
    let call = &requests[1]["messages"][1]["content"][0];
    assert_eq!(call["type"], "tool_use");
    assert_eq!(call["input"], json!({}));
}
