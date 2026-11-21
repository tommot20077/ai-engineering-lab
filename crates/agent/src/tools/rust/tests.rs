use std::sync::Mutex;

use futures::future::BoxFuture;
use jiff::Timestamp;
use sandbox::SandboxError;
use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::tool::{Clock, ErasedTool};

/// 假的沙箱:記下收到什麼,回傳事先給的結果。
struct Fake {
    reply: Result<Report, SandboxError>,
    received: Mutex<Vec<(String, Action)>>,
}

impl Sandbox for Fake {
    fn rust<'a>(
        &'a self,
        source: &'a str,
        action: Action,
    ) -> BoxFuture<'a, Result<Report, SandboxError>> {
        self.received
            .lock()
            .unwrap()
            .push((source.to_owned(), action));
        let reply = self.reply.clone();
        Box::pin(async move { reply })
    }
}

fn output(exit_code: Option<i32>, stdout: &str, stderr: &str) -> Output {
    Output {
        exit_code,
        stdout: stdout.into(),
        stderr: stderr.into(),
        timed_out: false,
        truncated: false,
    }
}

fn ok() -> Output {
    output(Some(0), "", "")
}

fn tool(reply: Result<Report, SandboxError>) -> (RunRust, Arc<Fake>) {
    let fake = Arc::new(Fake {
        reply,
        received: Mutex::new(Vec::new()),
    });
    (RunRust::new(fake.clone(), 20, 5), fake)
}

fn ctx() -> ToolContext {
    ToolContext {
        run_id: Uuid::nil(),
        call_id: "toolu_1".into(),
        clock: Clock::Fixed(Timestamp::UNIX_EPOCH),
    }
}

async fn run(reply: Result<Report, SandboxError>) -> Result<String, ToolError> {
    let (tool, _) = tool(reply);
    tool.call_json(&json!({"code": "fn main() {}"}).to_string(), &ctx())
        .await
}

#[tokio::test]
async fn the_code_goes_to_the_sandbox_to_be_compiled_and_run() {
    let (tool, fake) = tool(Ok(Report {
        compile: ok(),
        run: Some(output(Some(0), "42\n", "")),
    }));
    let code = "fn main() { println!(\"{}\", 6 * 7); }";
    let result = tool
        .call_json(&json!({ "code": code }).to_string(), &ctx())
        .await;
    assert_eq!(
        result,
        Ok("compile: ok\nrun: exit code 0\nstdout:\n42".to_string())
    );
    assert_eq!(
        *fake.received.lock().unwrap(),
        [(code.to_owned(), Action::Run)]
    );
}

#[tokio::test]
async fn a_compile_error_is_reported_with_rustc_output() {
    let report = Report {
        compile: output(Some(1), "", "error[E0425]: cannot find value `x`\n"),
        run: None,
    };
    assert_eq!(
        run(Ok(report)).await,
        Ok(
            "compile: failed (exit code 1)\nstderr:\nerror[E0425]: cannot find value `x`"
                .to_string()
        )
    );
    // 沙箱不守約定(編譯失敗卻附上執行結果):不把執行結果交給模型,只說編譯失敗。
    let odd = Report {
        compile: output(Some(1), "", "error\n"),
        run: Some(output(Some(0), "stale\n", "")),
    };
    assert_eq!(
        run(Ok(odd)).await,
        Ok("compile: failed (exit code 1)\nstderr:\nerror".to_string())
    );
}

#[tokio::test]
async fn every_way_a_program_can_end_is_described() {
    let mut timed_out = output(None, "start\n", "");
    timed_out.timed_out = true;
    let mut truncated = output(Some(0), "xxxx", "");
    truncated.truncated = true;
    for (run_output, expected) in [
        (
            output(Some(3), "out\n", "err\n"),
            "run: exit code 3\nstdout:\nout\nstderr:\nerr",
        ),
        (timed_out, "run: timed out and was stopped\nstdout:\nstart"),
        (
            output(Some(137), "", ""),
            "run: exit code 137: killed (probably out of memory)",
        ),
        (output(None, "", ""), "run: killed by a signal"),
        (
            truncated,
            "run: exit code 0\nstdout:\nxxxx\n(output truncated)",
        ),
    ] {
        let report = Report {
            compile: ok(),
            run: Some(run_output),
        };
        assert_eq!(
            run(Ok(report)).await,
            Ok(format!("compile: ok\n{expected}"))
        );
    }
    let mut slow = output(None, "", "");
    slow.timed_out = true;
    assert_eq!(
        run(Ok(Report {
            compile: slow,
            run: None
        }))
        .await,
        Ok("compile: failed (timed out and was stopped)".to_string())
    );
}

#[tokio::test]
async fn a_broken_sandbox_is_a_tool_error() {
    let error = SandboxError::ImageMissing {
        program: "docker".into(),
        image: "rust@sha256:x".into(),
    };
    assert_eq!(
        run(Err(error)).await,
        Err(ToolError::Failed(
            "the sandbox image is not available locally; run `docker pull rust@sha256:x` first"
                .into()
        ))
    );
}

#[tokio::test]
async fn the_approval_shows_the_numbered_code() {
    let (tool, fake) = tool(Ok(Report {
        compile: ok(),
        run: None,
    }));
    let shown = tool
        .describe_json(
            &json!({"code": "fn main() {\n    loop {}\n}"}).to_string(),
            &ctx(),
        )
        .await;
    assert_eq!(
        shown,
        "compile and run this Rust program in the sandbox (no network, no access to your files):\n\
         \x20  1 | fn main() {\n   2 |     loop {}\n   3 | }"
    );
    // 說明不會執行任何東西。
    assert!(fake.received.lock().unwrap().is_empty());
    assert!(ErasedTool::needs_approval(&tool));
}

#[test]
fn the_definition_sent_to_the_model_is_locked() {
    let (tool, _) = tool(Ok(Report {
        compile: ok(),
        run: None,
    }));
    insta::assert_json_snapshot!("run_rust", tool.def());
}

#[tokio::test]
async fn the_approval_shows_long_programs_in_full_up_to_16_kib() {
    // 一百行、每行約 30 bytes(約 3 KB):全部顯示,沒有「...」。
    let code: String = (0..100)
        .map(|n| format!("    let value_{n:03} = {n} * 2;\n"))
        .collect();
    let (tool, _) = tool(Ok(Report {
        compile: ok(),
        run: None,
    }));
    let shown = tool
        .describe_json(&json!({ "code": code }).to_string(), &ctx())
        .await;
    assert!(
        shown.contains(" 100 |     let value_099 = 99 * 2;"),
        "{shown}"
    );
    assert!(!shown.contains("..."), "{shown}");
}
