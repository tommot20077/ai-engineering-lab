//! E2E:`run_rust` 與 `--allow-run`(003 第三部分第 5 步)。真的執行 agent binary,上游的 Claude 是 wiremock。
//!
//! 沙箱的容器指令換成一個假的 `docker`(shell script,`[sandbox] program`):不需要 docker 也能測整條路。
//! `SANDBOX_DOCKER=1` 時另外對真的容器跑一次。

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const KEY: &str = "sk-ant-e2e-fake-key";
const PATIENCE: Duration = Duration::from_secs(120);
const CODE: &str = "fn main() {\n    println!(\"{}\", 6 * 7);\n}\n";

/// 假的 docker:記下每次呼叫;`exec … /work/main` 印出 42。
const FAKE_DOCKER: &str = r#"#!/bin/sh
echo "$*" >> '__DIR__/calls'
case "$1" in
  run) echo fake-container ;;
  exec)
    if [ "$2" = "--interactive" ]; then cat > '__DIR__/main.rs'; exit 0; fi
    case "$3" in
      rustc) exit 0 ;;
      /work/main) echo 42 ;;
    esac ;;
esac
"#;

/// 假的 Claude:還沒有工具結果就要求 `run_rust`;有了就把結果原樣回答。
struct FakeClaude;

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
            Some(block) => sse(&[
                start(),
                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": format!("result: {}", block["content"].as_str().unwrap()) } }),
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 10 } }),
                json!({ "type": "message_stop" }),
            ]),
            None => sse(&[
                start(),
                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "run_rust", "input": {} } }),
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": json!({"code": CODE}).to_string() } }),
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 10 } }),
                json!({ "type": "message_stop" }),
            ]),
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

struct Lab {
    dir: TempDir,
    _upstream: MockServer,
}

impl Lab {
    /// `program`:`None` = 假的 docker(寫在暫存目錄裡);`Some` = 真的指令。`extra`:接在設定檔後面。
    async fn start(program: Option<&str>, extra: &str) -> Lab {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(FakeClaude)
            .mount(&upstream)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let program = match program {
            Some(program) => program.to_owned(),
            None => fake_docker(dir.path()),
        };
        let config = format!(
            "[aliases]\ndefault = \"claude-haiku-5-5\"\n[anthropic]\nbase_url = \"{}\"\n\
             [paths]\ndata_dir = \"data\"\nrecords = \"records\"\n[sandbox]\nprogram = \"{program}\"\n{extra}",
            upstream.uri()
        );
        std::fs::write(dir.path().join("agent.toml"), config).unwrap();
        Lab {
            dir,
            _upstream: upstream,
        }
    }

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

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// 寫出假的 docker,確定執行得起來才回傳(平行的測試在 fork 時可能短暫拿著寫檔的 fd:ETXTBSY,見 sandbox 的 fake_docker.rs)。
fn fake_docker(dir: &std::path::Path) -> String {
    let path = dir.join("docker");
    std::fs::write(&path, FAKE_DOCKER.replace("__DIR__", dir.to_str().unwrap())).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    loop {
        match std::process::Command::new(&path).arg("probe").status() {
            Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(Duration::from_millis(10)),
            other => {
                other.unwrap();
                break;
            }
        }
    }
    std::fs::remove_file(dir.join("calls")).unwrap();
    path.to_str().unwrap().to_owned()
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const RUN: [&str; 4] = ["run", "6 乘 7?", "--allow-run", "--no-record"];

#[tokio::test]
async fn an_approved_program_runs_in_the_sandbox() {
    let lab = Lab::start(None, "").await;
    let out = lab.agent(&RUN, "y\n").await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains(
            "── approval needed: run_rust (toolu_1)\n\
             compile and run this Rust program in the sandbox (no network, no access to your files):\n\
             \x20  1 | fn main() {\n   2 |     println!(\"{}\", 6 * 7);\n   3 | }\n"
        ),
        "{stderr}"
    );
    assert_eq!(
        stdout,
        "result: compile: ok\nrun: exit code 0\nstdout:\n42\n"
    );
    // 程式碼透過 stdin 交給容器;容器最後被收掉。
    assert_eq!(
        std::fs::read_to_string(lab.dir.path().join("main.rs")).unwrap(),
        CODE
    );
    let calls = lab.calls();
    assert!(
        calls[0].starts_with("run --detach --rm --name ai-lab-sandbox-"),
        "{calls:?}"
    );
    assert!(
        calls
            .last()
            .unwrap()
            .starts_with("rm --force ai-lab-sandbox-"),
        "{calls:?}"
    );
}

#[tokio::test]
async fn a_denied_program_never_reaches_the_sandbox() {
    let lab = Lab::start(None, "").await;
    let out = lab.agent(&RUN, "n\n").await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        stdout,
        "result: the user denied this tool call; it was not executed\n"
    );
    assert_eq!(lab.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn run_rust_is_offered_only_with_allow_run() {
    let lab = Lab::start(None, "").await;
    let out = lab.agent(&["run", "6 乘 7?", "--no-record"], "y\n").await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(stdout, "result: unknown tool \"run_rust\"\n");
    assert_eq!(lab.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn allow_run_needs_a_tool_timeout_longer_than_the_sandbox() {
    let lab = Lab::start(None, "[limits]\ntool_timeout_secs = 20\n").await;
    let out = lab.agent(&RUN, "y\n").await;
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out)
            .1
            .contains("limits.tool_timeout_secs (20) must be larger than"),
        "{:?}",
        text(&out)
    );
    // 不用 run_rust 時,同樣的設定照常可以用。
    let out = lab.agent(&["run", "?", "--no-record"], "").await;
    assert!(out.status.success(), "{:?}", text(&out));
}

#[tokio::test]
async fn a_real_container_compiles_and_runs_the_program() {
    if !std::env::var("SANDBOX_DOCKER").is_ok_and(|v| v == "1") {
        eprintln!("skipped: set SANDBOX_DOCKER=1 to run against a real container");
        return;
    }
    let lab = Lab::start(Some("docker"), "").await;
    let out = lab.agent(&RUN, "y\n").await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert_eq!(
        stdout,
        "result: compile: ok\nrun: exit code 0\nstdout:\n42\n"
    );
}
