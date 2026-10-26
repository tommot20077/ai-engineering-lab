//! Live test:打真的 Anthropic API,確認對方的格式跟我們的翻譯層還對得上。
//!
//! 會花錢、需要網路,所以標成 `#[ignore]`,平常的 `cargo test` 和 CI 都不會跑。手動執行:
//!
//! ```text
//! cargo test -p llm --test live_anthropic -- --ignored
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use llm::{
    AnthropicClient, AnthropicConfig, Content, LlmProvider, Message, ProviderMetadata, Request,
    Role, SecretString, StopKind,
};
use serde_json::json;

fn client() -> AnthropicClient {
    dotenvy::dotenv().ok();
    let key = std::env::var("ANTHROPIC_API_KEY")
        .expect("set ANTHROPIC_API_KEY (for example in .env) to run live tests");
    AnthropicClient::new(AnthropicConfig::new(SecretString::from(key))).unwrap()
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![Content::Text {
            text: text.into(),
            meta: ProviderMetadata::default(),
        }],
    }
}

fn request(messages: Vec<Message>) -> Request {
    // effort 調到 low:測試只需要確認格式,不需要模型想很久,也比較省錢。
    let mut options = ProviderMetadata::default();
    options.insert("anthropic", json!({ "output_config": { "effort": "low" } }));
    Request {
        model: "claude-sonnet-5-5".into(),
        system: Some("Answer as briefly as possible.".into()),
        messages,
        tools: vec![],
        max_tokens: 1024,
        provider_options: options,
    }
}

fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
#[ignore = "calls the real Anthropic API; run with --ignored"]
async fn live_two_turn_conversation() {
    let client = client();

    let first = client
        .complete(request(vec![user("Reply with exactly one word: pong")]))
        .await
        .unwrap();
    assert_eq!(first.stop.kind, StopKind::EndTurn, "stop: {:?}", first.stop);
    assert!(text_of(&first.content).to_lowercase().contains("pong"));
    assert!(first.usage.input_tokens > 0 && first.usage.output_tokens > 0);

    // 第二輪把第一輪的回答原樣放回對話:如果 thinking 簽章之類的資料在翻譯中遺失,
    // Anthropic 會拒絕這個請求。這是 translate.rs 那個 round-trip 測試在真實 API 上的版本。
    let second = client
        .complete(request(vec![
            user("Reply with exactly one word: pong"),
            Message {
                role: Role::Assistant,
                content: first.content,
            },
            user("Now reply with exactly one word: ping"),
        ]))
        .await
        .unwrap();
    assert_eq!(
        second.stop.kind,
        StopKind::EndTurn,
        "stop: {:?}",
        second.stop
    );
    assert!(text_of(&second.content).to_lowercase().contains("ping"));
}

#[tokio::test]
#[ignore = "calls the real Anthropic API; run with --ignored"]
async fn live_bad_key_is_a_non_retryable_401() {
    let client = AnthropicClient::new(AnthropicConfig::new(SecretString::from(
        "sk-ant-invalid".to_owned(),
    )))
    .unwrap();
    let err = client
        .complete(request(vec![user("hi")]))
        .await
        .unwrap_err();
    assert!(
        matches!(err, llm::LlmError::Http { status: 401, .. }),
        "{err:?}"
    );
    assert!(!err.is_retryable());
}
