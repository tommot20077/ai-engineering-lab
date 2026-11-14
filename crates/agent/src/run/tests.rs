use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;
use jiff::Timestamp;
use llm::{EventStream, LlmError, StopReason};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use super::*;
use crate::tool::{ErasedTool, Tool, ToolError};
use crate::tools::Calculator;

// ---- 假的 provider:照劇本回應,並記下每一次收到的請求 ----

enum Reply {
    Now(Response),
    /// 等一段時間才回(測試用暫停的時鐘,不會真的等)。
    After(Duration, Response),
    Fail(LlmError),
}

#[derive(Default)]
struct Scripted {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Request>>,
}

impl Scripted {
    fn new(replies: Vec<Reply>) -> Arc<Scripted> {
        Arc::new(Scripted {
            replies: Mutex::new(replies.into()),
            requests: Mutex::default(),
        })
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl LlmProvider for Scripted {
    fn stream(&self, _request: Request) -> EventStream {
        Box::pin(futures::stream::empty())
    }

    fn complete(&self, request: Request) -> BoxFuture<'_, llm::Result<Response>> {
        self.requests.lock().unwrap().push(request);
        let reply = self.replies.lock().unwrap().pop_front();
        Box::pin(async move {
            match reply {
                Some(Reply::Now(r)) => Ok(r),
                Some(Reply::After(wait, r)) => {
                    tokio::time::sleep(wait).await;
                    Ok(r)
                }
                Some(Reply::Fail(e)) => Err(e),
                None => Err(LlmError::Protocol("the script ran out".into())),
            }
        })
    }
}

/// 永遠要求呼叫工具的模型:每次一個新的 id、固定的用量。
struct Looping {
    calls: AtomicUsize,
    usage: Usage,
}

impl LlmProvider for Looping {
    fn stream(&self, _request: Request) -> EventStream {
        Box::pin(futures::stream::empty())
    }

    fn complete(&self, _request: Request) -> BoxFuture<'_, llm::Result<Response>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut response = tool_reply(&[(&format!("toolu_{n}"), "count", "{}")]);
        response.usage = self.usage;
        Box::pin(async move { Ok(response) })
    }
}

// ---- 回應的寫法 ----

fn usage() -> Usage {
    Usage {
        input_tokens: 100,
        output_tokens: 10,
        ..Usage::default()
    }
}

fn response(content: Vec<Content>, stop: StopKind, raw: &str) -> Response {
    Response {
        id: None,
        model: "claude-test".into(),
        content,
        stop: StopReason {
            kind: stop,
            raw: raw.into(),
        },
        usage: usage(),
        meta: ProviderMetadata::default(),
    }
}

fn text(text: &str) -> Content {
    Content::Text {
        text: text.into(),
        meta: ProviderMetadata::default(),
    }
}

fn call(id: &str, name: &str, args: &str) -> Content {
    Content::ToolCall {
        id: id.into(),
        name: name.into(),
        args_json: args.into(),
        meta: ProviderMetadata::default(),
    }
}

fn text_reply(answer: &str) -> Response {
    response(vec![text(answer)], StopKind::EndTurn, "end_turn")
}

fn tool_reply(calls: &[(&str, &str, &str)]) -> Response {
    let content = calls
        .iter()
        .map(|(id, name, args)| call(id, name, args))
        .collect();
    response(content, StopKind::ToolUse, "tool_use")
}

// ---- 測試用的工具 ----

/// 記下被呼叫幾次;回傳第幾次。
#[derive(Default)]
struct Count {
    calls: AtomicUsize,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

impl Tool for Count {
    type Args = NoArgs;
    fn name(&self) -> &str {
        "count"
    }
    fn description(&self) -> &str {
        "計數"
    }
    async fn call(&self, _args: NoArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        Ok((self.calls.fetch_add(1, Ordering::SeqCst) + 1).to_string())
    }
}

/// 等 `secs` 秒才回傳 `label`;用來測並行與逾時。
struct Sleep;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SleepArgs {
    secs: u64,
    label: String,
}

impl Tool for Sleep {
    type Args = SleepArgs;
    fn name(&self) -> &str {
        "sleep"
    }
    fn description(&self) -> &str {
        "等一下"
    }
    async fn call(&self, args: SleepArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        tokio::time::sleep(Duration::from_secs(args.secs)).await;
        Ok(args.label)
    }
}

/// 回傳 `n` 個 `unit`;用來測截斷。
struct Repeat;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RepeatArgs {
    unit: String,
    n: usize,
}

impl Tool for Repeat {
    type Args = RepeatArgs;
    fn name(&self) -> &str {
        "repeat"
    }
    fn description(&self) -> &str {
        "重複"
    }
    async fn call(&self, args: RepeatArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        Ok(args.unit.repeat(args.n))
    }
}

/// 回傳 ToolContext 的內容:確認 run id、call id、時鐘有交給工具。
struct Whoami;

impl Tool for Whoami {
    type Args = NoArgs;
    fn name(&self) -> &str {
        "whoami"
    }
    fn description(&self) -> &str {
        "看 context"
    }
    async fn call(&self, _args: NoArgs, ctx: &ToolContext) -> Result<String, ToolError> {
        Ok(format!(
            "{} {} {}",
            ctx.run_id,
            ctx.call_id,
            ctx.clock.now()
        ))
    }
}

struct Setup {
    count: Arc<Count>,
}

/// 工具組:calculator、count、sleep、repeat、whoami。`count` 用 Arc 共用,測試才看得到它被呼叫幾次。
fn toolbox() -> (Arc<Toolbox>, Setup) {
    let count = Arc::new(Count::default());
    let tools: Vec<Box<dyn ErasedTool>> = vec![
        Box::new(Calculator),
        Box::new(Shared(count.clone())),
        Box::new(Sleep),
        Box::new(Repeat),
        Box::new(Whoami),
    ];
    (Arc::new(Toolbox::new(tools).unwrap()), Setup { count })
}

/// 讓 `Arc<Count>` 也是工具(轉給裡面那個)。
struct Shared(Arc<Count>);

impl Tool for Shared {
    type Args = NoArgs;
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    async fn call(&self, args: NoArgs, ctx: &ToolContext) -> Result<String, ToolError> {
        self.0.call(args, ctx).await
    }
}

fn config() -> AgentConfig {
    let mut config = AgentConfig::new("test-model");
    config.system = Some("你是助理".into());
    config.max_tokens = 1000;
    config.clock = Clock::Fixed("2026-11-14T02:30:00Z".parse::<Timestamp>().unwrap());
    config
}

fn build(provider: Arc<dyn LlmProvider>, config: AgentConfig) -> (Agent, Setup) {
    let (toolbox, setup) = toolbox();
    (Agent::new(provider, toolbox, config).unwrap(), setup)
}

fn failed(failure: AgentFailure) -> Outcome {
    Outcome::Failed { failure }
}

fn limit(limit: Limit) -> Outcome {
    Outcome::LimitReached { limit }
}

// ---- 基本流程 ----

#[tokio::test]
async fn a_plain_answer_ends_the_run() {
    let provider = Scripted::new(vec![Reply::Now(text_reply("你好"))]);
    let (agent, _) = build(provider.clone(), config());
    let finished = agent.run("打個招呼").await;

    assert_eq!(
        finished.outcome,
        Outcome::Answered {
            text: "你好".into()
        }
    );
    assert_eq!(finished.steps.len(), 1);
    assert_eq!(finished.usage, usage());
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.model, "test-model");
    assert_eq!(request.system.as_deref(), Some("你是助理"));
    assert_eq!(request.max_tokens, 1000);
    assert_eq!(
        request.messages,
        [Message {
            role: Role::User,
            content: vec![text("打個招呼")]
        }]
    );
    let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["calculator", "count", "sleep", "repeat", "whoami"]);
}

#[tokio::test]
async fn a_tool_result_is_fed_back_to_the_model() {
    let provider = Scripted::new(vec![
        Reply::Now(tool_reply(&[(
            "toolu_1",
            "calculator",
            r#"{"expression": "6 * 7"}"#,
        )])),
        Reply::Now(text_reply("答案是 42")),
    ]);
    let (agent, _) = build(provider.clone(), config());
    let finished = agent.run("6 乘 7").await;

    assert_eq!(
        finished.outcome,
        Outcome::Answered {
            text: "答案是 42".into()
        }
    );
    assert_eq!(finished.steps.len(), 2);
    let record = &finished.steps[0].tool_calls[0];
    assert_eq!(
        (
            record.call_id.as_str(),
            record.name.as_str(),
            record.args_json.as_str(),
            record.result.as_str(),
            record.is_error
        ),
        (
            "toolu_1",
            "calculator",
            r#"{"expression": "6 * 7"}"#,
            "42",
            false
        )
    );
    assert!(finished.steps[1].tool_calls.is_empty());
    // 第二次請求:原本的問題、模型的工具呼叫、工具結果。
    let second = &provider.requests()[1];
    assert_eq!(
        second.messages,
        [
            Message {
                role: Role::User,
                content: vec![text("6 乘 7")]
            },
            Message {
                role: Role::Assistant,
                content: vec![call("toolu_1", "calculator", r#"{"expression": "6 * 7"}"#)]
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    call_id: "toolu_1".into(),
                    content: "42".into(),
                    is_error: false
                }]
            },
        ]
    );
    // 用量是兩輪的加總。
    assert_eq!(finished.usage.input_tokens, 200);
    assert_eq!(finished.usage.output_tokens, 20);
}

#[tokio::test]
async fn the_answer_joins_every_text_block_and_skips_thinking() {
    let reply = response(
        vec![
            Content::Thinking {
                text: "想一下".into(),
                meta: ProviderMetadata::default(),
            },
            text("第一段"),
            text("第二段"),
        ],
        StopKind::EndTurn,
        "end_turn",
    );
    let (agent, _) = build(Scripted::new(vec![Reply::Now(reply)]), config());
    assert_eq!(
        agent.run("?").await.outcome,
        Outcome::Answered {
            text: "第一段第二段".into()
        }
    );
}

#[tokio::test]
async fn thinking_blocks_go_back_to_the_model_unchanged() {
    let mut meta = ProviderMetadata::default();
    meta.insert("anthropic", json!({"signature": "sig-123"}));
    let thinking = Content::Thinking {
        text: "先算".into(),
        meta,
    };
    let first = response(
        vec![
            thinking.clone(),
            call("toolu_1", "calculator", r#"{"expression": "1+1"}"#),
        ],
        StopKind::ToolUse,
        "tool_use",
    );
    let provider = Scripted::new(vec![Reply::Now(first), Reply::Now(text_reply("2"))]);
    let (agent, _) = build(provider.clone(), config());
    agent.run("1+1").await;
    let assistant = &provider.requests()[1].messages[1];
    assert_eq!(
        assistant.content,
        [
            thinking,
            call("toolu_1", "calculator", r#"{"expression": "1+1"}"#)
        ]
    );
}

#[tokio::test]
async fn the_tool_gets_the_run_id_call_id_and_clock() {
    let provider = Scripted::new(vec![
        Reply::Now(tool_reply(&[("toolu_9", "whoami", "{}")])),
        Reply::Now(text_reply("ok")),
    ]);
    let (agent, _) = build(provider, config());
    let mut run = agent.start("你是誰");
    let run_id = run.run_id();
    let step = run.next().await.unwrap();
    assert_eq!(
        step.tool_calls[0].result,
        format!("{run_id} toolu_9 2026-11-14T02:30:00Z")
    );
    // UUID v7:開頭是時間。
    assert_eq!(run_id.get_version_num(), 7);
}

// ---- 工具的錯誤交回模型 ----

#[tokio::test]
async fn tool_errors_go_back_to_the_model_and_the_run_continues() {
    let provider = Scripted::new(vec![
        Reply::Now(tool_reply(&[
            ("toolu_1", "delete_everything", "{}"),
            ("toolu_2", "calculator", r#"{"expr": "1"}"#),
            ("toolu_3", "calculator", r#"{"expression": "1 / 0"}"#),
        ])),
        Reply::Now(text_reply("抱歉")),
    ]);
    let (agent, _) = build(provider.clone(), config());
    let finished = agent.run("?").await;
    assert_eq!(
        finished.outcome,
        Outcome::Answered {
            text: "抱歉".into()
        }
    );
    let results: Vec<(&str, bool)> = finished.steps[0]
        .tool_calls
        .iter()
        .map(|r| (r.result.as_str(), r.is_error))
        .collect();
    assert_eq!(
        results,
        [
            (r#"unknown tool "delete_everything""#, true),
            (
                "invalid arguments: unknown field `expr`, expected `expression`",
                true
            ),
            ("division by zero", true),
        ]
    );
    let back = &provider.requests()[1].messages[2].content;
    assert!(matches!(
        &back[0],
        Content::ToolResult { is_error: true, .. }
    ));
}

#[tokio::test]
async fn oversized_arguments_are_refused_without_parsing() {
    let mut config = config();
    config.limits.max_args_bytes = 30;
    // 剛好 30 bytes 可以;31 bytes 拒絕。
    let exact = r#"{"expression": "1 + 2 + 3 + 4"}"#;
    assert_eq!(exact.len(), 31);
    let fits = r#"{"expression": "1 + 2 + 3 + 4"}"#.replace(" + 4", "+ 4");
    assert_eq!(fits.len(), 30);
    let provider = Scripted::new(vec![
        Reply::Now(tool_reply(&[
            ("toolu_1", "calculator", &fits),
            ("toolu_2", "calculator", exact),
        ])),
        Reply::Now(text_reply("ok")),
    ]);
    let (agent, _) = build(provider, config);
    let finished = agent.run("?").await;
    let results: Vec<(&str, bool)> = finished.steps[0]
        .tool_calls
        .iter()
        .map(|r| (r.result.as_str(), r.is_error))
        .collect();
    assert_eq!(
        results,
        [("10", false), ("arguments are larger than 30 bytes", true)]
    );
}

// ---- 並行 ----

#[tokio::test(start_paused = true)]
async fn calls_in_one_turn_run_in_parallel_and_keep_their_order() {
    let mut config = config();
    config.limits.max_parallel = 2;
    // 四個呼叫、每次最多兩個同時跑:3 秒 + 1 秒 一批、2 秒 + 1 秒 一批。
    let provider = Scripted::new(vec![
        Reply::Now(tool_reply(&[
            ("t1", "sleep", r#"{"secs": 3, "label": "a"}"#),
            ("t2", "sleep", r#"{"secs": 1, "label": "b"}"#),
            ("t3", "sleep", r#"{"secs": 2, "label": "c"}"#),
            ("t4", "sleep", r#"{"secs": 1, "label": "d"}"#),
        ])),
        Reply::Now(text_reply("ok")),
    ]);
    let (agent, _) = build(provider, config);
    let started = Instant::now();
    let finished = agent.run("?").await;
    let labels: Vec<&str> = finished.steps[0]
        .tool_calls
        .iter()
        .map(|r| r.result.as_str())
        .collect();
    // 結果照呼叫的順序,不是完成的順序。
    assert_eq!(labels, ["a", "b", "c", "d"]);
    // 一個一個跑要 7 秒;全部同時跑要 3 秒;最多兩個同時跑是 4 秒(a 3 秒的同時,b、c 接著跑完;d 等 c)。
    assert_eq!(started.elapsed(), Duration::from_secs(4));
}

#[tokio::test(start_paused = true)]
async fn a_slow_tool_times_out_into_an_error_result() {
    let mut config = config();
    config.limits.tool_timeout_secs = 30;
    let provider = Scripted::new(vec![
        Reply::Now(tool_reply(&[
            ("t1", "sleep", r#"{"secs": 30, "label": "on time"}"#),
            ("t2", "sleep", r#"{"secs": 31, "label": "late"}"#),
        ])),
        Reply::Now(text_reply("ok")),
    ]);
    let (agent, _) = build(provider, config);
    let finished = agent.run("?").await;
    let results: Vec<(&str, bool)> = finished.steps[0]
        .tool_calls
        .iter()
        .map(|r| (r.result.as_str(), r.is_error))
        .collect();
    assert_eq!(
        results,
        [
            ("on time", false),
            ("the tool did not finish within 30 seconds", true)
        ]
    );
}

// ---- 截斷 ----

#[tokio::test]
async fn large_results_are_cut_at_a_character_boundary() {
    let mut config = config();
    config.limits.max_result_bytes = 10;
    let provider = Scripted::new(vec![
        Reply::Now(tool_reply(&[
            ("t1", "repeat", r#"{"unit": "a", "n": 10}"#),
            ("t2", "repeat", r#"{"unit": "a", "n": 11}"#),
            // 「中」是 3 bytes:10 bytes 的上限會切在 9 bytes(3 個字),不會切進第 4 個字的中間。
            ("t3", "repeat", r#"{"unit": "中", "n": 5}"#),
        ])),
        Reply::Now(text_reply("ok")),
    ]);
    let (agent, _) = build(provider, config);
    let finished = agent.run("?").await;
    let results: Vec<&str> = finished.steps[0]
        .tool_calls
        .iter()
        .map(|r| r.result.as_str())
        .collect();
    assert_eq!(
        results,
        [
            "aaaaaaaaaa",
            "aaaaaaaaaa\n[truncated: 1 bytes omitted]",
            "中中中\n[truncated: 6 bytes omitted]",
        ]
    );
}

// ---- 停止原因 ----

#[tokio::test]
async fn stop_reasons_other_than_end_turn_and_tool_use_end_the_run() {
    let cases = [
        (
            StopKind::MaxTokens,
            "max_tokens",
            failed(AgentFailure::Truncated),
        ),
        (StopKind::Refusal, "refusal", Outcome::Refused),
        (
            StopKind::PauseTurn,
            "pause_turn",
            failed(AgentFailure::UnexpectedStop {
                raw: "pause_turn".into(),
            }),
        ),
        (
            StopKind::StopSequence,
            "stop_sequence",
            failed(AgentFailure::UnexpectedStop {
                raw: "stop_sequence".into(),
            }),
        ),
        (
            StopKind::Unknown,
            "brand_new",
            failed(AgentFailure::UnexpectedStop {
                raw: "brand_new".into(),
            }),
        ),
    ];
    for (kind, raw, expected) in cases {
        // 回應裡帶著工具呼叫:確認這些停止原因下,工具一律不執行。
        let reply = response(vec![text("…"), call("t1", "count", "{}")], kind, raw);
        let (agent, setup) = build(Scripted::new(vec![Reply::Now(reply)]), config());
        let finished = agent.run("?").await;
        assert_eq!(finished.outcome, expected, "{raw}");
        assert_eq!(finished.steps.len(), 1, "{raw}");
        assert_eq!(setup.count.calls.load(Ordering::SeqCst), 0, "{raw}");
    }
}

#[tokio::test]
async fn contradictions_between_stop_reason_and_content_are_failures() {
    // 有工具呼叫,停止原因卻是 end_turn:不執行。
    let reply = response(
        vec![call("t1", "count", "{}")],
        StopKind::EndTurn,
        "end_turn",
    );
    let (agent, setup) = build(Scripted::new(vec![Reply::Now(reply)]), config());
    assert_eq!(
        agent.run("?").await.outcome,
        failed(AgentFailure::CallsWithoutToolUse)
    );
    assert_eq!(setup.count.calls.load(Ordering::SeqCst), 0);

    // 停止原因是 tool_use,卻沒有工具呼叫。
    let reply = response(vec![text("…")], StopKind::ToolUse, "tool_use");
    let (agent, _) = build(Scripted::new(vec![Reply::Now(reply)]), config());
    assert_eq!(
        agent.run("?").await.outcome,
        failed(AgentFailure::ToolUseWithoutCalls)
    );
}

#[tokio::test]
async fn a_repeated_call_id_is_a_failure_and_is_not_executed() {
    // 跨輪重複。
    let provider = Scripted::new(vec![
        Reply::Now(tool_reply(&[("same", "count", "{}")])),
        Reply::Now(tool_reply(&[("same", "count", "{}")])),
    ]);
    let (agent, setup) = build(provider, config());
    let finished = agent.run("?").await;
    assert_eq!(
        finished.outcome,
        failed(AgentFailure::DuplicateCallId { id: "same".into() })
    );
    assert_eq!(setup.count.calls.load(Ordering::SeqCst), 1);

    // 同一輪裡重複:整輪都不執行。
    let provider = Scripted::new(vec![Reply::Now(tool_reply(&[
        ("a", "count", "{}"),
        ("b", "count", "{}"),
        ("a", "count", "{}"),
    ]))]);
    let (agent, setup) = build(provider, config());
    assert_eq!(
        agent.run("?").await.outcome,
        failed(AgentFailure::DuplicateCallId { id: "a".into() })
    );
    assert_eq!(setup.count.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failed_model_call_ends_the_run_without_a_step() {
    let provider = Scripted::new(vec![Reply::Fail(LlmError::Timeout)]);
    let (agent, _) = build(provider, config());
    let finished = agent.run("?").await;
    assert_eq!(
        finished.outcome,
        failed(AgentFailure::Model {
            message: "request timed out".into()
        })
    );
    assert!(finished.steps.is_empty());
}

// ---- 護欄 ----

fn looping(usage: Usage) -> Arc<Looping> {
    Arc::new(Looping {
        calls: AtomicUsize::new(0),
        usage,
    })
}

#[tokio::test]
async fn the_turn_limit_stops_a_model_that_never_answers() {
    let mut config = config();
    config.limits.max_turns = 3;
    let provider = looping(usage());
    let (agent, setup) = build(provider.clone(), config);
    let finished = agent.run("?").await;
    assert_eq!(finished.outcome, limit(Limit::Turns));
    assert_eq!(finished.steps.len(), 3);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    assert_eq!(setup.count.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn the_tool_call_limit_refuses_a_whole_turn_that_would_cross_it() {
    let mut config = config();
    config.limits.max_tool_calls = 3;
    let provider = Scripted::new(vec![
        Reply::Now(tool_reply(&[("a", "count", "{}"), ("b", "count", "{}")])),
        // 2 + 1 = 3:剛好到上限,可以執行。
        Reply::Now(tool_reply(&[("c", "count", "{}")])),
        // 3 + 1 > 3:整輪不執行。
        Reply::Now(tool_reply(&[("d", "count", "{}")])),
    ]);
    let (agent, setup) = build(provider, config);
    let finished = agent.run("?").await;
    assert_eq!(finished.outcome, limit(Limit::ToolCalls));
    assert_eq!(finished.steps.len(), 3);
    assert!(finished.steps[2].tool_calls.is_empty());
    assert_eq!(setup.count.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn the_token_limit_stops_before_the_next_model_call() {
    let mut config = config();
    // 每輪 600 tokens(含 cache):第一輪後 600 < 1000 繼續,第二輪後 1200 ≥ 1000 停。
    config.limits.max_total_tokens = 1000;
    let provider = looping(Usage {
        input_tokens: 300,
        output_tokens: 100,
        cache_read_tokens: 150,
        cache_write_tokens: 50,
    });
    let (agent, _) = build(provider.clone(), config);
    let finished = agent.run("?").await;
    assert_eq!(finished.outcome, limit(Limit::Tokens));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(finished.usage.cache_read_tokens, 300);
    assert_eq!(finished.usage.cache_write_tokens, 100);

    // 剛好等於上限也停。
    let mut config = self::config();
    config.limits.max_total_tokens = 600;
    let provider = looping(Usage {
        input_tokens: 600,
        ..Usage::default()
    });
    let (agent, _) = build(provider.clone(), config);
    assert_eq!(agent.run("?").await.outcome, limit(Limit::Tokens));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn the_deadline_cuts_off_a_slow_model_call() {
    let mut config = config();
    config.limits.deadline_secs = 300;
    let wait = Duration::from_secs(200);
    let provider = Scripted::new(vec![
        Reply::After(wait, tool_reply(&[("a", "count", "{}")])),
        Reply::After(wait, text_reply("太晚了")),
    ]);
    let (agent, _) = build(provider, config);
    let started = Instant::now();
    let finished = agent.run("?").await;
    assert_eq!(finished.outcome, limit(Limit::Deadline));
    assert_eq!(finished.steps.len(), 1);
    // 第二次呼叫在第 300 秒被取消,不是等到第 400 秒。
    assert_eq!(started.elapsed(), Duration::from_secs(300));
}

// ---- 一步一步拉 ----

#[tokio::test]
async fn steps_can_be_pulled_one_at_a_time() {
    let provider = Scripted::new(vec![
        Reply::Now(tool_reply(&[("a", "count", "{}")])),
        Reply::Now(text_reply("完成")),
    ]);
    let (agent, _) = build(provider.clone(), config());
    let mut run = agent.start("?");
    let first = run.next().await.unwrap();
    assert_eq!(first.turn, 1);
    assert_eq!(first.tool_calls[0].result, "1");
    let second = run.next().await.unwrap();
    assert_eq!(second.turn, 2);
    // 結束之後不再呼叫模型。
    assert!(run.next().await.is_none());
    assert!(run.next().await.is_none());
    assert_eq!(provider.requests().len(), 2);
    let finished = run.finish();
    assert_eq!(
        finished.outcome,
        Outcome::Answered {
            text: "完成".into()
        }
    );
    assert_eq!(finished.steps, [first, second]);
}

#[tokio::test]
async fn finishing_early_is_recorded_as_stopped() {
    let provider = Scripted::new(vec![Reply::Now(tool_reply(&[("a", "count", "{}")]))]);
    let (agent, _) = build(provider, config());
    let mut run = agent.start("?");
    run.next().await.unwrap();
    let finished = run.finish();
    assert_eq!(finished.outcome, Outcome::Stopped);
    assert_eq!(finished.steps.len(), 1);
}

// ---- 設定 ----

#[test]
fn bad_configurations_are_rejected_up_front() {
    let (toolbox, _) = toolbox();
    let provider: Arc<dyn LlmProvider> = Scripted::new(vec![]);
    let build = |config| Agent::new(provider.clone(), toolbox.clone(), config).err();

    let mut c = config();
    c.model = "  ".into();
    assert_eq!(build(c), Some(ConfigError::EmptyModel));
    let mut c = config();
    c.max_tokens = 0;
    assert_eq!(build(c), Some(ConfigError::ZeroMaxTokens));
    let mut c = config();
    c.limits = AgentLimits {
        max_turns: 0,
        max_tool_calls: 0,
        max_parallel: 0,
        max_total_tokens: 0,
        deadline_secs: 0,
        tool_timeout_secs: 0,
        max_result_bytes: 0,
        max_args_bytes: 0,
    };
    let error = build(c).unwrap();
    assert_eq!(
        error.to_string(),
        "agent limits must be at least 1: max_turns, max_tool_calls, max_parallel, \
         max_total_tokens, deadline_secs, tool_timeout_secs, max_result_bytes, max_args_bytes"
    );
    assert!(build(config()).is_none());
}

#[test]
fn the_default_limits_match_the_design() {
    assert_eq!(
        AgentLimits::default(),
        AgentLimits {
            max_turns: 10,
            max_tool_calls: 30,
            max_parallel: 4,
            max_total_tokens: 200_000,
            deadline_secs: 300,
            tool_timeout_secs: 30,
            max_result_bytes: 32 * 1024,
            max_args_bytes: 64 * 1024,
        }
    );
}

#[test]
fn outcomes_serialize_with_a_kind_tag() {
    assert_eq!(
        serde_json::to_value(limit(Limit::ToolCalls)).unwrap(),
        json!({"kind": "limit_reached", "limit": "tool_calls"})
    );
    assert_eq!(
        serde_json::to_value(failed(AgentFailure::DuplicateCallId { id: "x".into() })).unwrap(),
        json!({"kind": "failed", "failure": {"reason": "duplicate_call_id", "id": "x"}})
    );
}
