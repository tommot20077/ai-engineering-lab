//! 不需要 API key 的完整示範:用 library 跑完 `run` → `score` → `compare`,印出比較報告。
//!
//! ```text
//! cargo run -p eval --example offline
//! ```
//!
//! 兩個「模型」都是這個檔案裡的假 provider:`cheap` 答錯 JSON 那一題,`default` 全對。
//! 結果寫在暫存目錄,程式結束就刪掉。真的呼叫模型請用 CLI(見 crates/eval/README.md)。

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use eval::compare;
use eval::run::{self, ExecOptions, RunConfig};
use eval::score::{self, ScoreSetup};
use futures::future::BoxFuture;
use jiff::Timestamp;
use llm::{
    Content, EventStream, LlmProvider, ProviderMetadata, Request, Response, StopKind, StopReason,
    Usage,
};

/// 假的模型:看題目決定回答。`cheap` 的 JSON 少了 `version` 欄位。
struct Fake;

impl LlmProvider for Fake {
    // `complete` 已經自己實作,用不到串流。
    fn stream(&self, _request: Request) -> EventStream {
        Box::pin(futures::stream::empty())
    }

    fn complete(&self, request: Request) -> BoxFuture<'_, llm::Result<Response>> {
        let question = match request.messages.first().and_then(|m| m.content.first()) {
            Some(Content::Text { text, .. }) => text.clone(),
            _ => String::new(),
        };
        let cheap = request.model == "cheap";
        let answer = if question.contains("2 + 3") {
            "5"
        } else if question.contains("JSON object") {
            if cheap {
                r#"{"name":"serde"}"#
            } else {
                r#"{"name":"serde","version":"1.0.228"}"#
            }
        } else if question.contains("keyword") {
            "let"
        } else {
            "Paris"
        };
        Box::pin(futures::future::ready(Ok(Response {
            id: None,
            model: format!("fake-{}", request.model),
            content: vec![Content::Text {
                text: answer.into(),
                meta: ProviderMetadata::default(),
            }],
            stop: StopReason {
                kind: StopKind::EndTurn,
                raw: "end_turn".into(),
            },
            usage: Usage::default(),
            meta: ProviderMetadata::default(),
        })))
    }
}

fn config(model: &str) -> RunConfig {
    RunConfig {
        model: model.into(),
        repeats: 1,
        tags: vec![],
        limit: None,
        label: None,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let runs = tmp.path().join("runs");
    let dataset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/smoke.jsonl");
    let options = ExecOptions {
        concurrency: 2,
        timeout: Duration::from_secs(30),
        budget_usd: None,
        price: None,
    };

    // 1. 執行:對資料集的每一題呼叫模型,寫進 responses.jsonl。
    let (a, _) = run::start(
        &runs,
        &dataset,
        config("cheap"),
        options,
        &Fake,
        None,
        Timestamp::now(),
    )
    .await?;
    let (b, _) = run::start(
        &runs,
        &dataset,
        config("default"),
        options,
        &Fake,
        None,
        Timestamp::now(),
    )
    .await?;

    // 2. 打分:smoke 只有確定性檢查,不需要評審。
    let setup = ScoreSetup {
        name: "checks",
        judge: None,
        skip_on_check_failure: true,
    };
    for dir in [&a, &b] {
        score::score(dir.path(), &setup, options, Timestamp::now()).await?;
    }

    // 3. 比較:依案例配對,算兩個指標與 bootstrap 信賴區間。
    let report = compare::compare(
        &compare::load(a.path(), "checks")?,
        &compare::load(b.path(), "checks")?,
        false,
    )?;
    println!("{report}");
    Ok(())
}
