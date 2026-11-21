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

fn build(provider: &dyn LlmProvider, config: AgentConfig) -> (Agent<'_>, Setup) {
    let (toolbox, setup) = toolbox();
    (Agent::new(provider, toolbox, config).unwrap(), setup)
}

/// 下一步一定要是完成的一輪(這些測試的工具都不用核准)。
async fn next_step(run: &mut Run<'_>) -> Option<Step> {
    match run.next().await? {
        RunEvent::Step(step) => Some(step),
        RunEvent::NeedsApproval(pending) => panic!("unexpected approval request: {pending:?}"),
    }
}

/// 拉到結束。最多 50 步:錯的實作可能永遠停在「等核准」,`next()` 又不會讓出執行緒(tokio 的逾時插不進來),
/// 這裡數步數,讓測試失敗而不是卡住(Mutants 在 CI 上就是卡到 300 秒逾時)。
async fn drain(run: &mut Run<'_>) {
    for _ in 0..50 {
        if run.next().await.is_none() {
            return;
        }
    }
    panic!("the run did not finish within 50 events");
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
    let (agent, _) = build(&*provider, config());
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
    let (agent, _) = build(&*provider, config());
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
    let provider = Scripted::new(vec![Reply::Now(reply)]);
    let (agent, _) = build(&*provider, config());
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
    let (agent, _) = build(&*provider, config());
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
    let (agent, _) = build(&*provider, config());
    let mut run = agent.start("你是誰");
    let run_id = run.run_id();
    let step = next_step(&mut run).await.unwrap();
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
    let (agent, _) = build(&*provider, config());
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
    let (agent, _) = build(&*provider, config);
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
    let (agent, _) = build(&*provider, config);
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
    let (agent, _) = build(&*provider, config);
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
    // 工具耗時:準時的 30 秒,逾時的在第 30 秒被切斷。
    let durations: Vec<u64> = finished.steps[0]
        .tool_calls
        .iter()
        .map(|r| r.duration_ms)
        .collect();
    assert_eq!(durations, [30_000, 30_000]);
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
    let (agent, _) = build(&*provider, config);
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
        let provider = Scripted::new(vec![Reply::Now(reply)]);
        let (agent, setup) = build(&*provider, config());
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
    let provider = Scripted::new(vec![Reply::Now(reply)]);
    let (agent, setup) = build(&*provider, config());
    assert_eq!(
        agent.run("?").await.outcome,
        failed(AgentFailure::CallsWithoutToolUse)
    );
    assert_eq!(setup.count.calls.load(Ordering::SeqCst), 0);

    // 停止原因是 tool_use,卻沒有工具呼叫。
    let reply = response(vec![text("…")], StopKind::ToolUse, "tool_use");
    let provider = Scripted::new(vec![Reply::Now(reply)]);
    let (agent, _) = build(&*provider, config());
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
    let (agent, setup) = build(&*provider, config());
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
    let (agent, setup) = build(&*provider, config());
    assert_eq!(
        agent.run("?").await.outcome,
        failed(AgentFailure::DuplicateCallId { id: "a".into() })
    );
    assert_eq!(setup.count.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failed_model_call_ends_the_run_without_a_step() {
    let provider = Scripted::new(vec![Reply::Fail(LlmError::Timeout)]);
    let (agent, _) = build(&*provider, config());
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
    let (agent, setup) = build(&*provider, config);
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
    let (agent, setup) = build(&*provider, config);
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
    let (agent, _) = build(&*provider, config);
    let finished = agent.run("?").await;
    assert_eq!(finished.outcome, limit(Limit::Tokens));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(finished.usage.cache_read_tokens, 300);
    assert_eq!(finished.usage.cache_write_tokens, 100);

    // 只有 cache 寫入也算進合計:一輪 1000 個 cache 寫入的 token,上限 1000,第一輪後就停。
    let mut config = self::config();
    config.limits.max_total_tokens = 1000;
    let provider = looping(Usage {
        cache_write_tokens: 1000,
        ..Usage::default()
    });
    let (agent, _) = build(&*provider, config);
    assert_eq!(agent.run("?").await.outcome, limit(Limit::Tokens));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

    // 剛好等於上限也停。
    let mut config = self::config();
    config.limits.max_total_tokens = 600;
    let provider = looping(Usage {
        input_tokens: 600,
        ..Usage::default()
    });
    let (agent, _) = build(&*provider, config);
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
    let (agent, _) = build(&*provider, config);
    let started = Instant::now();
    let finished = agent.run("?").await;
    assert_eq!(finished.outcome, limit(Limit::Deadline));
    assert_eq!(finished.steps.len(), 1);
    // 延遲是這一步模型呼叫花的時間(暫停的時鐘:剛好 200 秒)。
    assert_eq!(finished.steps[0].latency_ms, 200_000);
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
    let (agent, _) = build(&*provider, config());
    let mut run = agent.start("?");
    let first = next_step(&mut run).await.unwrap();
    assert_eq!(first.turn, 1);
    assert_eq!(first.tool_calls[0].result, "1");
    let second = next_step(&mut run).await.unwrap();
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
    let (agent, _) = build(&*provider, config());
    let mut run = agent.start("?");
    next_step(&mut run).await.unwrap();
    let finished = run.finish();
    assert_eq!(finished.outcome, Outcome::Stopped);
    assert_eq!(finished.steps.len(), 1);
}

// ---- 核准(第三部分,D-056、D-057)----

/// 有副作用的工具:記下做過什麼;要核准,說明「會做什麼」。
struct Danger {
    done: Arc<Mutex<Vec<String>>>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DangerArgs {
    target: String,
}

impl Tool for Danger {
    type Args = DangerArgs;
    fn name(&self) -> &str {
        "danger"
    }
    fn description(&self) -> &str {
        "有副作用"
    }
    async fn call(&self, args: DangerArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        self.done.lock().unwrap().push(args.target.clone());
        Ok(format!("did {}", args.target))
    }
    fn needs_approval(&self) -> bool {
        true
    }
    async fn describe(&self, args: &DangerArgs, ctx: &ToolContext) -> Option<String> {
        Some(format!("will touch {} ({})", args.target, ctx.call_id))
    }
}

struct Guarded {
    done: Arc<Mutex<Vec<String>>>,
    count: Arc<Count>,
}

/// 工具組:count(不用核准)、danger(要核准)。
fn guarded(provider: &dyn LlmProvider, approval: ApprovalPolicy) -> (Agent<'_>, Guarded) {
    let done = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(Count::default());
    let tools: Vec<Box<dyn ErasedTool>> = vec![
        Box::new(Shared(count.clone())),
        Box::new(Danger { done: done.clone() }),
    ];
    let mut config = config();
    config.approval = approval;
    let agent = Agent::new(provider, Arc::new(Toolbox::new(tools).unwrap()), config).unwrap();
    (agent, Guarded { done, count })
}

fn pending(call_id: &str, target: &str) -> Approval {
    Approval {
        call_id: call_id.into(),
        tool: "danger".into(),
        args_json: format!(r#"{{"target":"{target}"}}"#),
        description: format!("will touch {target} ({call_id})"),
    }
}

fn danger(id: &'static str, target: &str) -> (&'static str, &'static str, String) {
    (id, "danger", format!(r#"{{"target":"{target}"}}"#))
}

fn reply_with(calls: &[(&str, &str, String)]) -> Response {
    let calls: Vec<(&str, &str, &str)> =
        calls.iter().map(|(i, n, a)| (*i, *n, a.as_str())).collect();
    tool_reply(&calls)
}

#[tokio::test]
async fn a_side_effect_waits_for_approval_and_runs_once_approved() {
    let provider = Scripted::new(vec![
        Reply::Now(reply_with(&[danger("a", "notes.md")])),
        Reply::Now(text_reply("寫好了")),
    ]);
    let (agent, g) = guarded(&*provider, ApprovalPolicy::Ask);
    let mut run = agent.start("?");

    assert_eq!(
        run.next().await,
        Some(RunEvent::NeedsApproval(vec![pending("a", "notes.md")]))
    );
    assert_eq!(run.pending(), [pending("a", "notes.md")]);
    // 還沒核准:什麼都沒做;再拉一次拿到同一份清單,不會多呼叫模型、也不會執行。
    assert_eq!(
        run.next().await,
        Some(RunEvent::NeedsApproval(vec![pending("a", "notes.md")]))
    );
    assert!(g.done.lock().unwrap().is_empty());
    assert_eq!(provider.requests().len(), 1);

    run.resolve(vec![("a".into(), Decision::Approve)]).unwrap();
    assert_eq!(run.pending(), []);
    let Some(RunEvent::Step(step)) = run.next().await else {
        panic!("expected the turn to finish")
    };
    assert_eq!(step.turn, 1);
    assert_eq!(step.tool_calls[0].result, "did notes.md");
    assert!(!step.tool_calls[0].is_error);
    assert_eq!(*g.done.lock().unwrap(), ["notes.md"]);

    assert!(matches!(run.next().await, Some(RunEvent::Step(_))));
    let finished = run.finish();
    assert_eq!(
        finished.outcome,
        Outcome::Answered {
            text: "寫好了".into()
        }
    );
    // 結果有交回模型。
    let second = &provider.requests()[1];
    assert_eq!(
        second.messages[2].content,
        [Content::ToolResult {
            call_id: "a".into(),
            content: "did notes.md".into(),
            is_error: false
        }]
    );
}

#[tokio::test]
async fn a_denied_call_is_not_run_and_the_model_is_told() {
    let provider = Scripted::new(vec![
        Reply::Now(reply_with(&[danger("a", "notes.md")])),
        Reply::Now(text_reply("好,不寫")),
    ]);
    let (agent, g) = guarded(&*provider, ApprovalPolicy::Ask);
    let mut run = agent.start("?");
    run.next().await.unwrap();
    run.resolve(vec![("a".into(), Decision::Deny)]).unwrap();
    let Some(RunEvent::Step(step)) = run.next().await else {
        panic!("expected the turn to finish")
    };
    assert_eq!(
        step.tool_calls,
        [ToolCallRecord {
            call_id: "a".into(),
            name: "danger".into(),
            args_json: r#"{"target":"notes.md"}"#.into(),
            result: DENIED.into(),
            is_error: true,
            duration_ms: 0,
        }]
    );
    assert!(g.done.lock().unwrap().is_empty());
    drain(&mut run).await;
    assert_eq!(
        provider.requests()[1].messages[2].content,
        [Content::ToolResult {
            call_id: "a".into(),
            content: DENIED.into(),
            is_error: true
        }]
    );
}

#[tokio::test]
async fn the_whole_turn_waits_and_keeps_its_order() {
    // count(不用核准)夾在兩個 danger 中間:核准一個、拒絕一個;三個結果照呼叫的順序。
    let provider = Scripted::new(vec![
        Reply::Now(reply_with(&[
            danger("a", "x"),
            ("b", "count", "{}".into()),
            danger("c", "y"),
        ])),
        Reply::Now(text_reply("ok")),
    ]);
    let (agent, g) = guarded(&*provider, ApprovalPolicy::Ask);
    let mut run = agent.start("?");
    assert_eq!(
        run.next().await,
        Some(RunEvent::NeedsApproval(vec![
            pending("a", "x"),
            pending("c", "y")
        ]))
    );
    // 沒有副作用的也等:核准之前 count 一次都沒被呼叫。
    assert_eq!(g.count.calls.load(Ordering::SeqCst), 0);
    run.resolve(vec![
        ("c".into(), Decision::Deny),
        ("a".into(), Decision::Approve),
    ])
    .unwrap();
    let Some(RunEvent::Step(step)) = run.next().await else {
        panic!("expected the turn to finish")
    };
    let results: Vec<(&str, &str)> = step
        .tool_calls
        .iter()
        .map(|r| (r.call_id.as_str(), r.result.as_str()))
        .collect();
    assert_eq!(results, [("a", "did x"), ("b", "1"), ("c", DENIED)]);
    assert_eq!(*g.done.lock().unwrap(), ["x"]);
}

#[tokio::test]
async fn auto_approve_runs_side_effects_without_asking() {
    let provider = Scripted::new(vec![
        Reply::Now(reply_with(&[danger("a", "notes.md")])),
        Reply::Now(text_reply("寫好了")),
    ]);
    let (agent, g) = guarded(&*provider, ApprovalPolicy::AutoApprove);
    let mut run = agent.start("?");
    let Some(RunEvent::Step(step)) = run.next().await else {
        panic!("auto-approve must not ask")
    };
    assert_eq!(step.tool_calls[0].result, "did notes.md");
    assert_eq!(*g.done.lock().unwrap(), ["notes.md"]);
}

#[tokio::test]
async fn run_to_completion_denies_what_nobody_can_approve() {
    let provider = Scripted::new(vec![
        Reply::Now(reply_with(&[danger("a", "notes.md")])),
        Reply::Now(text_reply("好,不寫")),
    ]);
    let (agent, g) = guarded(&*provider, ApprovalPolicy::Ask);
    let finished = agent.run("?").await;
    assert_eq!(finished.steps[0].tool_calls[0].result, DENIED);
    assert!(g.done.lock().unwrap().is_empty());
    assert_eq!(
        finished.outcome,
        Outcome::Answered {
            text: "好,不寫".into()
        }
    );
}

#[tokio::test]
async fn answers_must_match_the_pending_calls_exactly() {
    let provider = Scripted::new(vec![Reply::Now(reply_with(&[
        danger("a", "x"),
        danger("b", "y"),
    ]))]);
    let (agent, g) = guarded(&*provider, ApprovalPolicy::Ask);
    let mut run = agent.start("?");
    assert_eq!(
        run.resolve(vec![("a".into(), Decision::Approve)]),
        Err(ResolveError::NotWaiting),
        "nothing is pending before the model asks"
    );
    run.next().await.unwrap();
    let approve = |id: &str| (id.to_owned(), Decision::Approve);
    assert_eq!(
        run.resolve(vec![approve("a")]),
        Err(ResolveError::Missing("b".into()))
    );
    assert_eq!(
        run.resolve(vec![approve("a"), approve("b"), approve("z")]),
        Err(ResolveError::Unknown("z".into()))
    );
    assert_eq!(
        run.resolve(vec![approve("a"), approve("a"), approve("b")]),
        Err(ResolveError::Duplicate("a".into()))
    );
    // 錯的回答什麼都沒改:還在等同一份清單。
    assert_eq!(run.pending(), [pending("a", "x"), pending("b", "y")]);
    assert!(matches!(run.next().await, Some(RunEvent::NeedsApproval(_))));
    assert!(g.done.lock().unwrap().is_empty());
    // 回答過一次之後,不能再回答。
    run.resolve(vec![approve("a"), approve("b")]).unwrap();
    assert_eq!(
        run.resolve(vec![approve("a"), approve("b")]),
        Err(ResolveError::NotWaiting)
    );
}

#[tokio::test(start_paused = true)]
async fn time_spent_waiting_for_approval_does_not_count_toward_the_deadline() {
    let provider = Scripted::new(vec![
        Reply::After(Duration::from_secs(4), reply_with(&[danger("a", "x")])),
        Reply::After(Duration::from_secs(4), text_reply("趕上了")),
    ]);
    let (mut agent, _) = guarded(&*provider, ApprovalPolicy::Ask);
    agent.config.limits.deadline_secs = 10;
    let mut run = agent.start("?");
    run.next().await.unwrap();
    // 人去喝了一小時咖啡。
    tokio::time::sleep(Duration::from_secs(3600)).await;
    run.resolve(vec![("a".into(), Decision::Approve)]).unwrap();
    drain(&mut run).await;
    // 真的在跑的時間是 4 + 4 = 8 秒,在 10 秒以內。
    assert_eq!(
        run.finish().outcome,
        Outcome::Answered {
            text: "趕上了".into()
        }
    );
}

#[tokio::test(start_paused = true)]
async fn time_before_the_pause_still_counts_toward_the_deadline() {
    let provider = Scripted::new(vec![
        Reply::After(Duration::from_secs(6), reply_with(&[danger("a", "x")])),
        Reply::After(Duration::from_secs(6), text_reply("太晚了")),
    ]);
    let (mut agent, _) = guarded(&*provider, ApprovalPolicy::Ask);
    agent.config.limits.deadline_secs = 10;
    let mut run = agent.start("?");
    run.next().await.unwrap();
    tokio::time::sleep(Duration::from_secs(3600)).await;
    run.resolve(vec![("a".into(), Decision::Approve)]).unwrap();
    let resumed = Instant::now();
    drain(&mut run).await;
    // 暫停前用了 6 秒,只剩 4 秒:第二次呼叫在接續後第 4 秒被取消。
    assert_eq!(run.finish().outcome, limit(Limit::Deadline));
    assert_eq!(resumed.elapsed(), Duration::from_secs(4));
}

#[tokio::test]
async fn finishing_while_waiting_is_stopped_and_keeps_the_paid_turn() {
    let provider = Scripted::new(vec![Reply::Now(reply_with(&[danger("a", "x")]))]);
    let (agent, g) = guarded(&*provider, ApprovalPolicy::Ask);
    let mut run = agent.start("?");
    run.next().await.unwrap();
    let finished = run.finish();
    assert_eq!(finished.outcome, Outcome::Stopped);
    // 模型的回應付過錢了(用量算在裡面),紀錄也要有;工具沒有執行。
    assert_eq!(finished.steps.len(), 1);
    assert_eq!(finished.steps[0].tool_calls, []);
    assert_eq!(finished.usage, usage());
    assert!(g.done.lock().unwrap().is_empty());
}

#[tokio::test]
async fn oversized_arguments_are_refused_without_asking() {
    let provider = Scripted::new(vec![
        Reply::Now(reply_with(&[danger("a", &"x".repeat(100))])),
        Reply::Now(text_reply("ok")),
    ]);
    let (mut agent, g) = guarded(&*provider, ApprovalPolicy::Ask);
    agent.config.limits.max_args_bytes = 50;
    let mut run = agent.start("?");
    let Some(RunEvent::Step(step)) = run.next().await else {
        panic!("oversized arguments must not be shown for approval")
    };
    assert_eq!(
        step.tool_calls[0].result,
        "arguments are larger than 50 bytes"
    );
    assert!(g.done.lock().unwrap().is_empty());
}

#[tokio::test]
async fn arguments_exactly_at_the_limit_are_still_asked_about() {
    let args = r#"{"target":"x"}"#;
    let provider = Scripted::new(vec![Reply::Now(tool_reply(&[("a", "danger", args)]))]);
    let (mut agent, _) = guarded(&*provider, ApprovalPolicy::Ask);
    agent.config.limits.max_args_bytes = args.len();
    let mut run = agent.start("?");
    assert!(matches!(run.next().await, Some(RunEvent::NeedsApproval(_))));
}

#[tokio::test]
async fn the_tool_call_limit_is_checked_before_asking() {
    let provider = Scripted::new(vec![Reply::Now(reply_with(&[
        danger("a", "x"),
        danger("b", "y"),
    ]))]);
    let (mut agent, _) = guarded(&*provider, ApprovalPolicy::Ask);
    agent.config.limits.max_tool_calls = 1;
    let mut run = agent.start("?");
    // 這一輪本來就不會執行:不要拿來問人。
    assert!(matches!(run.next().await, Some(RunEvent::Step(_))));
    assert_eq!(run.finish().outcome, limit(Limit::ToolCalls));
}

#[tokio::test]
async fn the_description_shown_for_invalid_arguments_includes_them() {
    let provider = Scripted::new(vec![Reply::Now(tool_reply(&[(
        "a",
        "danger",
        r#"{"path":"x"}"#,
    )]))]);
    let (agent, _) = guarded(&*provider, ApprovalPolicy::Ask);
    let mut run = agent.start("?");
    let Some(RunEvent::NeedsApproval(pending)) = run.next().await else {
        panic!("expected an approval request")
    };
    assert_eq!(
        pending[0].description,
        "invalid arguments: unknown field `path`, expected `target`\narguments: {\"path\":\"x\"}"
    );
}

// ---- 存檔與接續(第 4 步,D-057)----

/// 一個停在等核准的執行,存檔出來。
async fn paused(provider: &dyn LlmProvider) -> Suspended {
    let (agent, _) = guarded(provider, ApprovalPolicy::Ask);
    let mut run = agent.start("記下來");
    assert!(matches!(run.next().await, Some(RunEvent::NeedsApproval(_))));
    run.suspend().unwrap()
}

#[tokio::test]
async fn a_waiting_run_can_be_suspended_and_then_it_is_over() {
    let provider = Scripted::new(vec![Reply::Now(reply_with(&[danger("a", "x")]))]);
    let (agent, g) = guarded(&*provider, ApprovalPolicy::Ask);
    let mut run = agent.start("記下來");
    run.next().await.unwrap();
    let saved = run.suspend().unwrap();
    assert_eq!(saved.version, SUSPENDED_VERSION);
    assert_eq!(saved.run_id, run.run_id());
    assert_eq!(saved.model, "test-model");
    assert_eq!(saved.system.as_deref(), Some("你是助理"));
    assert_eq!(
        saved.messages,
        [Message {
            role: Role::User,
            content: vec![text("記下來")]
        }]
    );
    assert_eq!(saved.steps, []);
    assert_eq!(saved.waiting.turn, 1);
    assert_eq!(saved.pending, [pending("a", "x")]);
    assert_eq!(saved.usage, usage());
    // 這個 Run 結束了;那一輪在存檔裡,不在結果裡。
    assert_eq!(run.next().await, None);
    let finished = run.finish();
    assert_eq!(finished.outcome, Outcome::Stopped);
    assert_eq!(finished.steps, []);
    assert!(g.done.lock().unwrap().is_empty());
}

#[tokio::test]
async fn only_a_run_waiting_for_answers_can_be_suspended() {
    let provider = Scripted::new(vec![
        Reply::Now(reply_with(&[danger("a", "x")])),
        Reply::Now(text_reply("ok")),
    ]);
    let (agent, _) = guarded(&*provider, ApprovalPolicy::Ask);
    let mut run = agent.start("?");
    assert_eq!(run.suspend(), None, "not started yet");
    run.next().await.unwrap();
    run.resolve(vec![("a".into(), Decision::Approve)]).unwrap();
    assert_eq!(run.suspend(), None, "already answered");
    // 沒有被動到:照常跑完。
    drain(&mut run).await;
    assert_eq!(
        run.finish().outcome,
        Outcome::Answered { text: "ok".into() }
    );
}

#[tokio::test]
async fn a_suspended_run_survives_a_round_trip_through_json() {
    let provider = Scripted::new(vec![Reply::Now(reply_with(&[danger("a", "x")]))]);
    let saved = paused(&*provider).await;
    let json = serde_json::to_string(&saved).unwrap();
    assert_eq!(serde_json::from_str::<Suspended>(&json).unwrap(), saved);
    // 認不得的欄位不默默忽略。
    let mut value = serde_json::to_value(&saved).unwrap();
    value["surprise"] = json!(1);
    assert!(serde_json::from_value::<Suspended>(value).is_err());
}

#[tokio::test]
async fn a_resumed_run_asks_again_and_continues_where_it_stopped() {
    let first = Scripted::new(vec![Reply::Now(reply_with(&[danger("a", "x")]))]);
    let saved = paused(&*first).await;
    // 程式重開:新的 provider、新的 agent。
    let second = Scripted::new(vec![Reply::Now(text_reply("寫好了"))]);
    let (agent, g) = guarded(&*second, ApprovalPolicy::Ask);
    let mut run = agent.resume(saved.clone()).await.unwrap();
    assert_eq!(run.run_id(), saved.run_id);
    assert_eq!(
        run.next().await,
        Some(RunEvent::NeedsApproval(vec![pending("a", "x")]))
    );
    run.resolve(vec![("a".into(), Decision::Approve)]).unwrap();
    let Some(RunEvent::Step(step)) = run.next().await else {
        panic!("expected the paused turn to finish")
    };
    assert_eq!(
        (step.turn, step.tool_calls[0].result.as_str()),
        (1, "did x")
    );
    assert_eq!(*g.done.lock().unwrap(), ["x"]);
    drain(&mut run).await;
    let finished = run.finish();
    assert_eq!(
        finished.outcome,
        Outcome::Answered {
            text: "寫好了".into()
        }
    );
    assert_eq!(finished.steps.len(), 2);
    // 用量從暫停前接著加。
    assert_eq!(finished.usage.input_tokens, 200);
    // 新的 provider 收到的是完整的對話:任務、模型要呼叫工具、工具結果。
    assert_eq!(first.requests().len(), 1);
    let request = &second.requests()[0];
    assert_eq!(request.messages.len(), 3);
    assert_eq!(request.messages[0].content, [text("記下來")]);
    assert_eq!(
        request.messages[2].content,
        [Content::ToolResult {
            call_id: "a".into(),
            content: "did x".into(),
            is_error: false
        }]
    );
}

#[tokio::test]
async fn resuming_recomputes_what_needs_approval() {
    // 存檔被改過(等核准的清單被清空):照樣要問。
    let first = Scripted::new(vec![Reply::Now(reply_with(&[danger("a", "x")]))]);
    let mut saved = paused(&*first).await;
    saved.pending.clear();
    let second = Scripted::new(vec![]);
    let (agent, g) = guarded(&*second, ApprovalPolicy::Ask);
    let mut run = agent.resume(saved).await.unwrap();
    assert_eq!(
        run.next().await,
        Some(RunEvent::NeedsApproval(vec![pending("a", "x")]))
    );
    assert!(g.done.lock().unwrap().is_empty());
}

#[tokio::test]
async fn resuming_with_auto_approve_runs_the_paused_turn_without_asking() {
    let first = Scripted::new(vec![Reply::Now(reply_with(&[danger("a", "x")]))]);
    let saved = paused(&*first).await;
    let second = Scripted::new(vec![Reply::Now(text_reply("ok"))]);
    let (agent, g) = guarded(&*second, ApprovalPolicy::AutoApprove);
    let mut run = agent.resume(saved).await.unwrap();
    assert!(matches!(run.next().await, Some(RunEvent::Step(_))));
    assert_eq!(*g.done.lock().unwrap(), ["x"]);
}

#[tokio::test]
async fn resuming_is_refused_when_the_setup_changed() {
    let first = Scripted::new(vec![Reply::Now(reply_with(&[danger("a", "x")]))]);
    let saved = paused(&*first).await;
    let second = Scripted::new(vec![]);
    let (agent, _) = guarded(&*second, ApprovalPolicy::Ask);
    let refused = |s: Suspended| {
        let agent = &agent;
        async move { agent.resume(s).await.err() }
    };

    let mut s = saved.clone();
    s.version = 2;
    assert_eq!(
        refused(s).await,
        Some(ResumeError::Version {
            found: 2,
            expected: 1
        })
    );
    let mut s = saved.clone();
    s.model = "other".into();
    assert_eq!(refused(s).await, Some(ResumeError::Changed("model")));
    let mut s = saved.clone();
    s.system = None;
    assert_eq!(
        refused(s).await,
        Some(ResumeError::Changed("system prompt"))
    );
    let mut s = saved.clone();
    s.max_tokens += 1;
    assert_eq!(refused(s).await, Some(ResumeError::Changed("max_tokens")));
    let mut s = saved.clone();
    s.limits.max_turns += 1;
    assert_eq!(refused(s).await, Some(ResumeError::Changed("limits")));
    let mut s = saved.clone();
    s.tools.pop();
    assert_eq!(refused(s).await, Some(ResumeError::Changed("tools")));
    let mut s = saved.clone();
    s.waiting.response = text_reply("沒有工具呼叫");
    assert_eq!(
        refused(s).await,
        Some(ResumeError::Invalid(
            "the paused turn has no tool calls".into()
        ))
    );
    assert!(refused(saved).await.is_none());
    assert_eq!(
        ResumeError::Changed("tools").to_string(),
        "the tools changed since the run was paused; start a new run instead"
    );
}

#[tokio::test(start_paused = true)]
async fn the_time_used_before_suspending_still_counts_after_resuming() {
    let first = Scripted::new(vec![Reply::After(
        Duration::from_secs(6),
        reply_with(&[danger("a", "x")]),
    )]);
    let (mut agent, _) = guarded(&*first, ApprovalPolicy::Ask);
    agent.config.limits.deadline_secs = 10;
    let mut run = agent.start("?");
    run.next().await.unwrap();
    let saved = run.suspend().unwrap();
    assert_eq!(saved.used_ms, 6000);

    tokio::time::sleep(Duration::from_secs(3600)).await;
    let second = Scripted::new(vec![Reply::After(
        Duration::from_secs(6),
        text_reply("太晚了"),
    )]);
    let (mut agent, _) = guarded(&*second, ApprovalPolicy::AutoApprove);
    agent.config.limits.deadline_secs = 10;
    let mut run = agent.resume(saved).await.unwrap();
    let resumed = Instant::now();
    drain(&mut run).await;
    assert_eq!(run.finish().outcome, limit(Limit::Deadline));
    assert_eq!(resumed.elapsed(), Duration::from_secs(4));
}

// ---- 設定 ----

#[test]
fn bad_configurations_are_rejected_up_front() {
    let (toolbox, _) = toolbox();
    let provider: Arc<dyn LlmProvider> = Scripted::new(vec![]);
    let build = |config| Agent::new(&*provider, toolbox.clone(), config).err();

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
