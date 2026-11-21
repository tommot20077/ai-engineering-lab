//! E2E:寫檔工具與核准提示(003 第三部分第 3 步)。真的執行 agent binary,上游的 Claude 是 wiremock,
//! 核准的回答從 stdin 送進去。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const KEY: &str = "sk-ant-e2e-fake-key";
const PATIENCE: Duration = Duration::from_secs(60);

/// 假的 Claude:還沒有工具結果時,要求呼叫 `tool`(參數 `args`);有了就把結果原樣回答出來。
struct FakeClaude {
    tool: &'static str,
    args: Value,
}

impl Respond for FakeClaude {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let result = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .find(|block| block["type"] == "tool_result");
        match result {
            Some(block) => text_sse(&format!("result: {}", block["content"].as_str().unwrap())),
            None => tool_sse("toolu_1", self.tool, &self.args.to_string()),
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
    json!({ "type": "message_start", "message": { "id": "msg_1", "model": "claude-haiku-5-5", "usage": { "input_tokens": 100, "output_tokens": 1 } } })
}

fn text_sse(text: &str) -> ResponseTemplate {
    sse(&[
        start(),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 10 } }),
        json!({ "type": "message_stop" }),
    ])
}

fn tool_sse(id: &str, name: &str, args: &str) -> ResponseTemplate {
    sse(&[
        start(),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": args } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 10 } }),
        json!({ "type": "message_stop" }),
    ])
}

struct Lab {
    dir: TempDir,
    _upstream: MockServer,
}

impl Lab {
    async fn start(tool: &'static str, args: Value) -> Lab {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(FakeClaude { tool, args })
            .mount(&upstream)
            .await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("notes")).unwrap();
        std::fs::write(dir.path().join("notes/todo.md"), "- 買牛奶\n").unwrap();
        let config = format!(
            "[aliases]\ndefault = \"claude-haiku-5-5\"\n[anthropic]\nbase_url = \"{}\"\n\
             [paths]\ndata_dir = \"data\"\nrecords = \"records\"\n",
            upstream.uri()
        );
        std::fs::write(dir.path().join("agent.toml"), config).unwrap();
        Lab {
            dir,
            _upstream: upstream,
        }
    }

    /// 執行 agent,`stdin` 是送進去的回答(送完就關掉)。
    async fn agent(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agent"))
            .args(args)
            .current_dir(self.dir.path())
            .env("ANTHROPIC_API_KEY", KEY)
            .env("RUST_BACKTRACE", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin.as_bytes()).await.unwrap();
        drop(input);
        tokio::time::timeout(PATIENCE, child.wait_with_output())
            .await
            .expect("agent did not finish in time")
            .unwrap()
    }

    fn todo(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("notes/todo.md")).unwrap()
    }

    fn trajectory(&self) -> Vec<Value> {
        let runs = self.dir.path().join("data/runs");
        let file = std::fs::read_dir(runs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const WRITE: [&str; 6] = [
    "run",
    "記下要買蛋",
    "--root",
    "notes",
    "--allow-write",
    "--no-record",
];

fn write_args() -> Value {
    json!({"path": "todo.md", "content": "- 買蛋\n"})
}

#[tokio::test]
async fn an_approved_write_happens_and_is_recorded() {
    let lab = Lab::start("write_file", write_args()).await;
    let out = lab.agent(&WRITE, "y\n").await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    // 核准提示:具體要做什麼。
    assert!(
        stderr.contains("── approval needed: write_file (toolu_1)\noverwrite todo.md (12 bytes -> 9 bytes)\n  - 買蛋\nallow? [y/N] "),
        "{stderr}"
    );
    assert_eq!(stdout, "result: overwrote todo.md (9 bytes)\n");
    assert_eq!(lab.todo(), "- 買蛋\n");
    // 軌跡:誰同意了什麼,寫在那一輪的 step 之前。
    let events = lab.trajectory();
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["start", "approval", "step", "step", "finish"]);
    assert_eq!(events[0]["auto_approve"], false);
    assert_eq!(
        events[1]["decisions"],
        json!([{"call_id": "toolu_1", "tool": "write_file", "decision": "approve"}])
    );
}

#[tokio::test]
async fn anything_but_yes_is_a_denial() {
    for answer in ["n\n", "\n", "sure\n", ""] {
        let lab = Lab::start("write_file", write_args()).await;
        let out = lab.agent(&WRITE, answer).await;
        let (stdout, stderr) = text(&out);
        assert!(out.status.success(), "{answer:?}: {stderr}");
        assert_eq!(
            stdout, "result: the user denied this tool call; it was not executed\n",
            "{answer:?}"
        );
        assert_eq!(lab.todo(), "- 買牛奶\n", "{answer:?}");
        let events = lab.trajectory();
        assert_eq!(events[1]["decisions"][0]["decision"], "deny", "{answer:?}");
    }
}

#[tokio::test]
async fn yes_writes_without_asking() {
    let lab = Lab::start("write_file", write_args()).await;
    let mut args = WRITE.to_vec();
    args.push("--yes");
    let out = lab.agent(&args, "").await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("approval needed"), "{stderr}");
    assert_eq!(stdout, "result: overwrote todo.md (9 bytes)\n");
    assert_eq!(lab.todo(), "- 買蛋\n");
    let events = lab.trajectory();
    assert_eq!(events[0]["auto_approve"], true);
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["start", "step", "step", "finish"]);
}

#[tokio::test]
async fn edit_file_shows_the_change_and_applies_it() {
    let lab = Lab::start(
        "edit_file",
        json!({"path": "todo.md", "old": "買牛奶", "new": "買豆漿"}),
    )
    .await;
    let out = lab.agent(&WRITE, "yes\n").await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("edit todo.md at line 1\n- 買牛奶\n+ 買豆漿\nallow? [y/N] "),
        "{stderr}"
    );
    assert_eq!(stdout, "result: edited todo.md at line 1\n");
    assert_eq!(lab.todo(), "- 買豆漿\n");
}

#[tokio::test]
async fn the_write_tools_are_not_offered_without_allow_write() {
    let lab = Lab::start("write_file", write_args()).await;
    let out = lab
        .agent(
            &["run", "記下要買蛋", "--root", "notes", "--no-record"],
            "y\n",
        )
        .await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    // 模型硬要呼叫也沒有這個工具:不會問、不會寫。
    assert!(!stderr.contains("approval needed"), "{stderr}");
    assert_eq!(stdout, "result: unknown tool \"write_file\"\n");
    assert_eq!(lab.todo(), "- 買牛奶\n");
}

#[tokio::test]
async fn allow_write_without_a_root_is_refused() {
    let lab = Lab::start("write_file", write_args()).await;
    let out = lab.agent(&["run", "?", "--allow-write"], "").await;
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).1.contains("--root"), "{:?}", text(&out));
}

#[tokio::test]
async fn control_characters_from_the_model_do_not_reach_the_terminal() {
    // 內容裡夾著清畫面的控制序列:提示裡看到的是 `?`,檔案裡寫的是原本的內容(核准的就是要寫的)。
    let lab = Lab::start(
        "write_file",
        json!({"path": "todo.md", "content": "ok\u{1b}[2J\n"}),
    )
    .await;
    let out = lab.agent(&WRITE, "y\n").await;
    let (_, stderr) = text(&out);
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    assert!(stderr.contains("  ok?[2J\n"), "{stderr}");
    assert_eq!(lab.todo(), "ok\u{1b}[2J\n");
}
