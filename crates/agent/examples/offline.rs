//! 不需要 API key 的 agent 範例:「模型」是照劇本回答的假 provider,工具是真的。
//!
//! ```text
//! cargo run -p agent --example offline
//! ```
//!
//! 看得到 agent loop 的完整流程:模型要求呼叫 calculator → 程式執行 → 結果接回去 → 模型回答。
//! 真的呼叫 Claude 用 `agent run`(見 crate README)。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use agent::tools::{Calculator, CurrentTime};
use agent::{Agent, AgentConfig, Clock, ErasedTool, Outcome, Toolbox};
use futures::future::BoxFuture;
use llm::{
    Content, EventStream, LlmError, LlmProvider, ProviderMetadata, Request, Response, StopKind,
    StopReason, Usage,
};

/// 照劇本回答的「模型」:第一次要求算 1234 * 5678,第二次看到結果後回答。
struct Scripted {
    replies: Mutex<VecDeque<Response>>,
}

impl LlmProvider for Scripted {
    // 這個範例只用 `complete`;`stream` 是 trait 規定一定要有的方法。
    fn stream(&self, _request: Request) -> EventStream {
        Box::pin(futures::stream::empty())
    }

    fn complete(&self, request: Request) -> BoxFuture<'_, llm::Result<Response>> {
        // 印出模型這一輪看到的最後一則訊息,看得出工具結果有接回去。
        if let Some(last) = request.messages.last() {
            for block in &last.content {
                if let Content::ToolResult { content, .. } = block {
                    println!("  模型收到工具結果:{content}");
                }
            }
        }
        // `.ok()` 先把鎖的結果換成 Option:`PoisonError` 裡還拿著鎖(MutexGuard),它不能跨執行緒(不是 `Send`),
        // 不能被放進下面要交給別人執行的 future。
        let reply = self.replies.lock().ok().and_then(|mut q| q.pop_front());
        Box::pin(
            async move { reply.ok_or_else(|| LlmError::Protocol("the script ran out".into())) },
        )
    }
}

fn response(content: Vec<Content>, kind: StopKind, raw: &str) -> Response {
    Response {
        id: None,
        model: "scripted".into(),
        content,
        stop: StopReason {
            kind,
            raw: raw.into(),
        },
        usage: Usage {
            input_tokens: 500,
            output_tokens: 30,
            ..Usage::default()
        },
        meta: ProviderMetadata::default(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let script = vec![
        response(
            vec![Content::ToolCall {
                id: "toolu_1".into(),
                name: "calculator".into(),
                args_json: r#"{"expression": "1234 * 5678"}"#.into(),
                meta: ProviderMetadata::default(),
            }],
            StopKind::ToolUse,
            "tool_use",
        ),
        response(
            vec![Content::Text {
                text: "1234 × 5678 = 7,006,652。".into(),
                meta: ProviderMetadata::default(),
            }],
            StopKind::EndTurn,
            "end_turn",
        ),
    ];
    let provider = Arc::new(Scripted {
        replies: Mutex::new(script.into()),
    });
    let tools: Vec<Box<dyn ErasedTool>> = vec![Box::new(Calculator), Box::new(CurrentTime)];
    let mut config = AgentConfig::new("scripted");
    config.clock = Clock::System;
    let agent = Agent::new(provider, Arc::new(Toolbox::new(tools)?), config)?;

    println!("任務:1234 乘 5678 是多少?");
    let mut run = agent.start("1234 乘 5678 是多少?");
    while let Some(step) = run.next().await {
        for call in &step.tool_calls {
            println!(
                "第 {} 輪:呼叫 {} {} → {}",
                step.turn, call.name, call.args_json, call.result
            );
        }
    }
    let finished = run.finish();
    match &finished.outcome {
        Outcome::Answered { text } => println!("回答:{text}"),
        other => println!("沒有回答:{other:?}"),
    }
    println!(
        "共 {} 輪,用量 {} input / {} output tokens",
        finished.steps.len(),
        finished.usage.input_tokens,
        finished.usage.output_tokens
    );
    Ok(())
}
