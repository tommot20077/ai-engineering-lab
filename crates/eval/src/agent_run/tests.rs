//! 一題 agent 經過整條 eval 執行流程(`run::start`):資料集 → agent loop(假的模型、真的工具)→ responses.jsonl。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use jiff::Timestamp;
use llm::{
    Content, EventStream, LlmError, ProviderMetadata, Request, Response, StopKind, StopReason,
    Usage,
};

use super::*;
use crate::run::{self, ExecOptions, Price, RunConfig, RunError};
use crate::store::{CallResult, RunDir};
use crate::{Dataset, Input, Output};

/// 假的模型。最後一則訊息有工具結果 → 回答「結果:<內容>」;沒有工具 → 直接回答;
/// 題目是 `fail` → 529;題目是 `file` → 要求讀 notes.txt;其他 → 要求查台北時間。
#[derive(Default)]
struct Scripted {
    calls: AtomicUsize,
}

fn first_text(request: &Request) -> String {
    match request.messages.first().and_then(|m| m.content.first()) {
        Some(Content::Text { text, .. }) => text.clone(),
        _ => String::new(),
    }
}

fn reply(content: Content, kind: StopKind, raw: &str) -> Response {
    Response {
        id: None,
        model: "claude-haiku-5-5".into(),
        content: vec![content],
        stop: StopReason {
            kind,
            raw: raw.into(),
        },
        usage: Usage {
            input_tokens: 1000,
            output_tokens: 100,
            ..Usage::default()
        },
        meta: ProviderMetadata::default(),
    }
}

fn text(text: String) -> Content {
    Content::Text {
        text,
        meta: ProviderMetadata::default(),
    }
}

impl LlmProvider for Scripted {
    fn stream(&self, _request: Request) -> EventStream {
        Box::pin(futures::stream::empty())
    }

    fn complete(&self, request: Request) -> BoxFuture<'_, llm::Result<Response>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let task = first_text(&request);
        let result = request.messages.last().and_then(|m| {
            m.content.iter().find_map(|c| match c {
                Content::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
        });
        let response = if task == "fail" {
            Err(LlmError::http(529, None, ""))
        } else if let Some(result) = result {
            Ok(reply(
                text(format!("結果:{result}")),
                StopKind::EndTurn,
                "end_turn",
            ))
        } else if request.tools.is_empty() {
            Ok(reply(
                text("沒有工具".into()),
                StopKind::EndTurn,
                "end_turn",
            ))
        } else {
            let (name, args) = if task == "file" {
                ("read_file", r#"{"path": "notes.txt"}"#)
            } else {
                ("current_time", r#"{"timezone": "Asia/Taipei"}"#)
            };
            Ok(reply(
                Content::ToolCall {
                    id: format!("toolu_{n}"),
                    name: name.into(),
                    args_json: args.into(),
                    meta: ProviderMetadata::default(),
                },
                StopKind::ToolUse,
                "tool_use",
            ))
        };
        Box::pin(async move { response })
    }
}

const PRICE: Price = Price {
    input: 2.0,
    output: 10.0,
};

fn agent_line(id: &str, input: serde_json::Value) -> String {
    serde_json::json!({
        "id": id,
        "input": input,
        "checks": [{"type": "contains", "value": "x"}],
    })
    .to_string()
}

/// 暫存目錄裡的資料集 `ds.jsonl`,以及 `fixtures/demo/notes.txt`。
fn setup(lines: &[String]) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("ds.jsonl");
    std::fs::write(&path, lines.join("\n")).unwrap();
    std::fs::create_dir_all(tmp.path().join("fixtures/demo")).unwrap();
    std::fs::write(tmp.path().join("fixtures/demo/notes.txt"), "hello").unwrap();
    let runs = tmp.path().join("runs");
    (tmp, path, runs)
}

fn config() -> RunConfig {
    RunConfig {
        model: "cheap".into(),
        repeats: 1,
        tags: vec![],
        limit: None,
        label: None,
    }
}

fn options() -> ExecOptions {
    ExecOptions {
        concurrency: 2,
        timeout: Duration::from_secs(300),
        budget_usd: None,
        price: Some(PRICE),
    }
}

fn now() -> Timestamp {
    "2026-11-14T02:30:00Z".parse().unwrap()
}

async fn run_one(lines: &[String], provider: &Scripted) -> Result<Vec<CallResult>, RunError> {
    let (_tmp, path, runs) = setup(lines);
    let (dir, _) = run::start(&runs, &path, config(), options(), provider, None, now()).await?;
    Ok(RunDir::open(dir.path())
        .unwrap()
        .read_responses()
        .unwrap()
        .into_iter()
        .map(|line| line.result)
        .collect())
}

fn agent_output(result: &CallResult) -> &AgentOutput {
    match result {
        CallResult::Ok {
            output: Output::Agent(output),
        } => output,
        other => panic!("expected an agent output, got {other:?}"),
    }
}

#[tokio::test]
async fn an_agent_case_runs_the_loop_with_a_fixed_clock() {
    let provider = Scripted::default();
    let line = agent_line(
        "time",
        serde_json::json!({"type": "agent", "prompt": "現在幾點?", "tools": ["current_time"],
                           "max_tokens": 256, "now": "2026-11-14T02:30:00Z"}),
    );
    let results = run_one(&[line], &provider).await.unwrap();
    let output = agent_output(&results[0]);
    assert_eq!(
        Output::Agent(output.clone()).text(),
        "結果:2026-11-14T10:30:00+08:00[Asia/Taipei], Saturday"
    );
    assert_eq!(output.steps.len(), 2);
    assert_eq!(output.model, "claude-haiku-5-5");
    // 用量是兩輪的加總。
    assert_eq!(output.usage.input_tokens, 2000);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn the_cost_of_an_agent_case_adds_up_every_turn() {
    let (_tmp, path, runs) = setup(&[agent_line(
        "time",
        serde_json::json!({"type": "agent", "prompt": "幾點?", "tools": ["current_time"], "max_tokens": 256}),
    )]);
    let provider = Scripted::default();
    let (_dir, summary) = run::start(&runs, &path, config(), options(), &provider, None, now())
        .await
        .unwrap();
    // 兩輪 × (1000 × $2 + 100 × $10) / 1M = 2 × 0.003。
    let cost = summary.cost_usd.unwrap();
    assert!((cost - 0.006).abs() < 1e-9, "{cost}");
}

#[tokio::test]
async fn file_tools_read_the_fixture_next_to_the_dataset() {
    let provider = Scripted::default();
    let line = agent_line(
        "file",
        serde_json::json!({"type": "agent", "prompt": "file", "tools": ["list_dir", "read_file"],
                           "root": "fixtures/demo", "max_tokens": 256}),
    );
    let results = run_one(&[line], &provider).await.unwrap();
    assert_eq!(
        Output::Agent(agent_output(&results[0]).clone()).text(),
        "結果:1\thello"
    );
}

#[tokio::test]
async fn without_tools_the_model_answers_directly() {
    let provider = Scripted::default();
    let line = agent_line(
        "plain",
        serde_json::json!({"type": "agent", "prompt": "幾點?", "max_tokens": 256}),
    );
    let results = run_one(&[line], &provider).await.unwrap();
    let output = agent_output(&results[0]);
    assert_eq!(Output::Agent(output.clone()).text(), "沒有工具");
    assert_eq!(output.steps.len(), 1);
}

#[tokio::test]
async fn a_failed_model_call_inside_the_agent_is_an_error_not_a_wrong_answer() {
    let provider = Scripted::default();
    let line = agent_line(
        "fail",
        serde_json::json!({"type": "agent", "prompt": "fail", "tools": ["calculator"], "max_tokens": 256}),
    );
    let results = run_one(&[line], &provider).await.unwrap();
    match &results[0] {
        CallResult::Error { error } => {
            assert_eq!(error.kind, "agent_model");
            assert_eq!(error.message, "provider returned HTTP 529");
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_missing_root_stops_the_run_before_any_model_call() {
    let provider = Scripted::default();
    let line = agent_line(
        "file",
        serde_json::json!({"type": "agent", "prompt": "file", "tools": ["read_file"],
                           "root": "fixtures/missing", "max_tokens": 256}),
    );
    let (_tmp, path, runs) = setup(&[line]);
    let error = run::start(&runs, &path, config(), options(), &provider, None, now())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&error, RunError::MissingRoot { case, path } if case == "file" && path.ends_with("fixtures/missing")),
        "{error}"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    // 連執行目錄都還沒建立。
    assert!(!runs.exists());
}

#[test]
fn the_estimate_covers_the_token_limit_and_every_turn_using_max_tokens() {
    let ds = Dataset::parse(
        "ds",
        agent_line(
            "a",
            serde_json::json!({"type": "agent", "system": "簡短", "prompt": "四個字的", "tools": ["calculator"],
                               "max_tokens": 1000, "max_turns": 3}),
        )
        .as_bytes(),
    )
    .unwrap();
    let Input::Agent(input) = &ds.cases[0].input else {
        panic!("expected an agent input");
    };
    let (tokens_in, tokens_out) = worst_case_tokens(input);
    // 60,000 + (12 bytes 的題目 + 6 bytes 的 system + 4096 bytes 的工具說明) ÷ 2;3 輪 × 1000。
    assert_eq!(tokens_in, 60_000.0 + (12.0 + 6.0 + 4096.0) / 2.0);
    assert_eq!(tokens_out, 3000.0);
    let items = run::plan(&ds, &config());
    let estimate = run::estimate(&ds, &items, Some(PRICE));
    let expected = (tokens_in * 2.0 + tokens_out * 10.0) / 1_000_000.0;
    assert!((estimate.worst_case_usd.unwrap() - expected).abs() < 1e-9);
}

#[test]
fn eval_limits_are_tighter_than_the_cli_defaults() {
    let ds = Dataset::parse(
        "ds",
        agent_line(
            "a",
            serde_json::json!({"type": "agent", "prompt": "q", "max_tokens": 100}),
        )
        .as_bytes(),
    )
    .unwrap();
    let Input::Agent(input) = &ds.cases[0].input else {
        panic!("expected an agent input");
    };
    let l = limits(input);
    assert_eq!((l.max_turns, l.max_total_tokens), (8, 60_000));
    let mut custom = input.clone();
    custom.max_turns = Some(3);
    assert_eq!(limits(&custom).max_turns, 3);
}

#[test]
fn an_agent_that_did_not_answer_has_no_text_to_check() {
    let output = |outcome| {
        Output::Agent(AgentOutput {
            model: "m".into(),
            outcome,
            steps: vec![],
            usage: Usage::default(),
        })
    };
    assert_eq!(
        output(agent::Outcome::Answered { text: "42".into() }).text(),
        "42"
    );
    // 撞到護欄、拒答:沒有回答,確定性檢查一律不通過。
    assert_eq!(
        output(agent::Outcome::LimitReached {
            limit: agent::Limit::Turns
        })
        .text(),
        ""
    );
    assert_eq!(output(agent::Outcome::Refused).text(), "");
    assert_eq!(output(agent::Outcome::Refused).model(), "m");
}

#[test]
fn the_debug_output_of_an_agent_input_hides_the_task() {
    let input = AgentInput {
        system: Some("SYSTEM-CANARY".into()),
        prompt: "PROMPT-CANARY".into(),
        tools: vec!["calculator".into()],
        max_tokens: 100,
        now: None,
        root: Some("fixtures/demo".into()),
        max_turns: Some(3),
    };
    let out = format!("{input:?}");
    assert!(!out.contains("CANARY"), "{out}");
    assert_eq!(
        out,
        r#"AgentInput { system_bytes: Some(13), prompt_bytes: 13, tools: ["calculator"], max_tokens: 100, now: None, root: Some("fixtures/demo"), max_turns: Some(3) }"#
    );
}
