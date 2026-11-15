//! E2E:agent binary 接上真的 MCP server(`mcp-test-server`),上游的 Claude 是 wiremock(design-part2 §7)。
//!
//! 驗證 composition root 的組裝:讀 `[[mcp]]`、啟動或連上 server、白名單、工具名稱的前綴、結果接回模型、
//! 環境變數不外漏、任何一個 server 有問題就在呼叫模型之前結束。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const KEY: &str = "sk-ant-e2e-fake-key";
const PATIENCE: Duration = Duration::from_secs(60);
/// HTTP 的 MCP server 要的 token 放在這個環境變數(agent.toml 的 `token_env`)。
const TOKEN_VAR: &str = "MCP_E2E_SECRET";

/// 測試 server 的執行檔。它屬於 `mcp` crate:`CARGO_BIN_EXE_*` 只給同一個 crate 的 binary,所以從 agent 的
/// 執行檔旁邊找(`cargo test --workspace` 會一起編好);找不到(只跑 `-p agent`)就先編譯一次。
fn mcp_test_server() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let agent = Path::new(env!("CARGO_BIN_EXE_agent"));
        let path = agent.with_file_name(format!("mcp-test-server{}", std::env::consts::EXE_SUFFIX));
        if !path.exists() {
            // 目錄名稱就是 profile(`debug` 是 `dev`)。
            let dir = agent
                .parent()
                .and_then(Path::file_name)
                .and_then(|d| d.to_str())
                .unwrap();
            let profile = if dir == "debug" { "dev" } else { dir };
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args([
                    "build",
                    "-p",
                    "mcp",
                    "--bin",
                    "mcp-test-server",
                    "--profile",
                    profile,
                ])
                .status()
                .unwrap();
            assert!(status.success(), "could not build mcp-test-server");
        }
        path
    })
}

/// 假的 Claude:對話裡還沒有工具結果,就呼叫 `tool`;有了就把結果原樣當成回答(測試看得到工具結果有沒有接回去)。
struct FakeClaude {
    tool: &'static str,
    args: &'static str,
    calls: AtomicUsize,
}

impl Respond for FakeClaude {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let result = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .find(|block| block["type"] == "tool_result");
        match result {
            Some(block) => {
                let text = match &block["content"] {
                    Value::String(s) => s.clone(),
                    other => other[0]["text"].as_str().unwrap_or_default().to_owned(),
                };
                text_sse(&format!("result: {text}"))
            }
            None => tool_sse(&format!("toolu_{n}"), self.tool, self.args),
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

struct Lab {
    dir: TempDir,
    upstream: MockServer,
}

impl Lab {
    async fn start(tool: &'static str, args: &'static str, mcp: &str) -> Lab {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(FakeClaude {
                tool,
                args,
                calls: AtomicUsize::new(0),
            })
            .mount(&upstream)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let config = format!(
            r#"
[aliases]
default = "claude-haiku-5-5"
[anthropic]
base_url = "{}"
[paths]
data_dir = "data"
records = "records"
{mcp}
"#,
            upstream.uri()
        );
        std::fs::write(dir.path().join("agent.toml"), config).unwrap();
        Lab { dir, upstream }
    }

    async fn agent(&self, token: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent"));
        cmd.args(["run", "use the tool", "--no-record"])
            .current_dir(self.dir.path())
            .env("ANTHROPIC_API_KEY", KEY)
            .env_remove(TOKEN_VAR)
            .env("RUST_BACKTRACE", "0");
        if let Some(token) = token {
            cmd.env(TOKEN_VAR, token);
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
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn stdio_server(name: &str, flags: &str, tools: &str) -> String {
    format!(
        "[[mcp]]\nname = \"{name}\"\ncommand = {:?}\nargs = [\"--stdio\"{flags}]\nenv = {{ MCP_TEST_FOO = \"bar\" }}\ntools = [{tools}]\n",
        mcp_test_server().to_str().unwrap()
    )
}

/// HTTP 模式的測試 server:URL 印在 stdout 的第一行。
async fn http_server(flags: &[&str]) -> (String, Child) {
    let mut child = Command::new(mcp_test_server())
        .args(["--http", "0"])
        .args(flags)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .await
        .unwrap();
    (line.trim().to_owned(), child)
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_stdio_server_tool_is_called_and_its_result_goes_back_to_the_model() {
    let lab = Lab::start(
        "test__add",
        r#"{"a": 2, "b": 3}"#,
        &stdio_server("test", "", "\"add\""),
    )
    .await;
    let out = lab.agent(None).await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(stdout.trim(), "result: 5");
    assert!(
        stderr.contains("mcp: `test` offers 12 tool(s), using 1"),
        "{stderr}"
    );
    // 白名單:只有 `test__add` 送給模型,server 的其他 11 個工具連名字都不出現(D-050)。
    let requests = lab.requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        tool_names(&requests[0]),
        ["calculator", "current_time", "test__add"]
    );
    let add = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "test__add")
        .unwrap();
    assert_eq!(add["description"], "Adds two integers.");
    assert_eq!(add["input_schema"]["required"], json!(["a", "b"]));
}

#[tokio::test]
async fn invalid_arguments_are_answered_without_calling_the_server() {
    // 模型給錯型別:McpTool 用 server 的 schema 擋下,錯誤交回模型(is_error),server 沒被呼叫。
    let lab = Lab::start(
        "test__add",
        r#"{"a": "two", "b": 3}"#,
        &stdio_server("test", "", "\"add\""),
    )
    .await;
    let out = lab.agent(None).await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        stdout.trim(),
        r#"result: invalid arguments: at /a: "two" is not of type "integer""#
    );
    let requests = lab.requests().await;
    let result = requests[1]["messages"][2]["content"][0].clone();
    assert_eq!(result["is_error"], true);
}

#[tokio::test]
async fn the_server_only_sees_the_allowed_environment() {
    // agent 有 ANTHROPIC_API_KEY;子行程只拿到 PATH、HOME、LANG(有設的話)與設定檔寫的 env。
    let lab = Lab::start(
        "test__env",
        "{}",
        &stdio_server("test", ", \"--env-tool\"", "\"env\""),
    )
    .await;
    let out = lab.agent(None).await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    let names: Vec<&str> = stdout
        .trim()
        .trim_start_matches("result: ")
        .split(',')
        .collect();
    assert!(names.contains(&"PATH"), "{names:?}");
    assert!(names.contains(&"MCP_TEST_FOO"), "{names:?}");
    assert!(!names.contains(&"ANTHROPIC_API_KEY"), "{names:?}");
    for name in &names {
        assert!(
            ["PATH", "HOME", "LANG", "MCP_TEST_FOO"].contains(name),
            "unexpected variable {name}"
        );
    }
}

#[tokio::test]
async fn an_http_server_with_a_token_works_the_same_way() {
    let (url, _server) = http_server(&["--token", "tok-e2e"]).await;
    let mcp = format!(
        "[[mcp]]\nname = \"remote\"\nurl = \"{url}\"\ntoken_env = \"{TOKEN_VAR}\"\ntools = [\"echo\"]\n"
    );
    let lab = Lab::start("remote__echo", r#"{"text": "over http"}"#, &mcp).await;
    let out = lab.agent(Some("tok-e2e")).await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(stdout.trim(), "result: over http");
}

#[tokio::test]
async fn a_missing_token_stops_before_calling_the_model() {
    let (url, _server) = http_server(&["--token", "tok-e2e"]).await;
    let mcp = format!(
        "[[mcp]]\nname = \"remote\"\nurl = \"{url}\"\ntoken_env = \"{TOKEN_VAR}\"\ntools = [\"echo\"]\n"
    );
    let lab = Lab::start("remote__echo", "{}", &mcp).await;
    let out = lab.agent(None).await;
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("MCP server `remote`"), "{stderr}");
    assert!(stderr.contains(TOKEN_VAR), "{stderr}");
    assert_eq!(lab.requests().await.len(), 0);
}

#[tokio::test]
async fn an_allowed_tool_the_server_lacks_stops_before_calling_the_model() {
    let lab = Lab::start(
        "test__add",
        "{}",
        &stdio_server("test", "", "\"add\", \"nope\""),
    )
    .await;
    let out = lab.agent(None).await;
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("MCP server `test` does not offer the tool `nope`"),
        "{stderr}"
    );
    assert_eq!(lab.requests().await.len(), 0);
}

#[tokio::test]
async fn a_legacy_server_stops_before_calling_the_model() {
    let lab = Lab::start(
        "test__add",
        "{}",
        &stdio_server("test", ", \"--legacy\"", "\"add\""),
    )
    .await;
    let out = lab.agent(None).await;
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("does not speak protocol version 2026-07-28"),
        "{stderr}"
    );
    assert_eq!(lab.requests().await.len(), 0);
}

#[tokio::test]
async fn a_command_that_does_not_exist_stops_before_calling_the_model() {
    let mcp = "[[mcp]]\nname = \"gone\"\ncommand = \"/nonexistent/mcp-server\"\ntools = [\"x\"]\n";
    let lab = Lab::start("gone__x", "{}", mcp).await;
    let out = lab.agent(None).await;
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("could not start the MCP server `/nonexistent/mcp-server`"),
        "{stderr}"
    );
    assert_eq!(lab.requests().await.len(), 0);
}
