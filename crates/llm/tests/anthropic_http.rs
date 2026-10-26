//! 整合測試:`AnthropicClient` 走真的 HTTP,對面是 wiremock 起的假伺服器。
//!
//! 只把「網路另一端」換成假的,我們自己的程式(reqwest、SSE 解析、翻譯、累加)全部跑真的。
// `tests/` 底下的每個檔案都是一個獨立的 crate,只能用 `llm` 公開的 API,
// 跟真正的使用者處境一樣。Java 對照:像是只能呼叫 public 方法的黑箱測試。

// 整個檔案都是測試,輔助函式也一樣:失敗就該 panic。
// (clippy.toml 的放寬只涵蓋 `#[test]` 函式,不涵蓋這裡的輔助函式。)
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use futures::StreamExt;
use llm::{
    AnthropicClient, AnthropicConfig, Content, LlmError, LlmProvider, Message, ProviderMetadata,
    Request, Role, SecretString, StopKind, StreamEvent,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request as WireRequest, ResponseTemplate};

const KEY: &str = "sk-ant-CANARY-7f3a9";

fn client(server: &MockServer) -> AnthropicClient {
    let mut config = AnthropicConfig::new(SecretString::from(KEY.to_owned()));
    config.base_url = server.uri();
    config.idle_timeout = Duration::from_millis(500);
    AnthropicClient::new(config).unwrap()
}

fn request(prompt: &str) -> Request {
    Request {
        model: "claude-sonnet-5-5".into(),
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: prompt.into(),
                meta: ProviderMetadata::default(),
            }],
        }],
        tools: vec![],
        max_tokens: 1024,
        provider_options: ProviderMetadata::default(),
    }
}

/// 把幾個 JSON 事件組成 Anthropic 的 SSE body。
fn sse_body(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn hello_events() -> Vec<Value> {
    vec![
        json!({ "type": "message_start", "message": { "id": "msg_1", "model": "claude-sonnet-5-5", "usage": { "input_tokens": 9, "output_tokens": 1 } } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hel" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "lo" } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 2 } }),
        json!({ "type": "message_stop" }),
    ]
}

fn sse_response(body: String) -> ResponseTemplate {
    // 注意:`set_body_string` 會把 content-type 蓋成 text/plain,要用 `set_body_raw` 指定。
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

#[tokio::test]
async fn complete_streams_and_collects_a_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", KEY))
        .and(header("anthropic-version", "2026-10-21"))
        .and(header("content-type", "application/json"))
        .respond_with(sse_response(sse_body(&hello_events())))
        .expect(1)
        .mount(&server)
        .await;

    let resp = client(&server).complete(request("hi")).await.unwrap();

    assert_eq!(
        resp.content,
        vec![Content::Text {
            text: "Hello".into(),
            meta: ProviderMetadata::default()
        }]
    );
    assert_eq!(resp.stop.kind, StopKind::EndTurn);
    assert_eq!((resp.usage.input_tokens, resp.usage.output_tokens), (9, 2));

    // 送出的 body 也檢查一下:一定要串流,而且帶著我們的模型名稱。
    let sent: Vec<WireRequest> = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["model"], "claude-sonnet-5-5");
}

#[tokio::test]
async fn stream_yields_events_as_they_arrive() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse_response(sse_body(&hello_events())))
        .mount(&server)
        .await;

    let events: Vec<StreamEvent> = client(&server)
        .stream(request("hi"))
        .map(|e| e.unwrap())
        .collect()
        .await;
    assert!(matches!(
        events.first(),
        Some(StreamEvent::MessageStart { .. })
    ));
    assert!(matches!(events.last(), Some(StreamEvent::Finish { .. })));
}

#[tokio::test]
async fn rate_limit_is_retryable_and_keeps_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "2")
                .set_body_string(r#"{"type":"error","error":{"type":"rate_limit_error"}}"#),
        )
        .mount(&server)
        .await;

    let err = client(&server).complete(request("hi")).await.unwrap_err();
    assert!(err.is_retryable());
    assert!(
        matches!(err, LlmError::Http { status: 429, retry_after: Some(d), .. } if d == Duration::from_secs(2)),
        "{err:?}"
    );
}

#[tokio::test]
async fn auth_failure_is_not_retryable_and_body_is_capped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("x".repeat(100_000)))
        .mount(&server)
        .await;

    let err = client(&server).complete(request("hi")).await.unwrap_err();
    assert!(!err.is_retryable());
    let LlmError::Http {
        status: 401, body, ..
    } = err
    else {
        panic!("expected 401, got {err:?}")
    };
    assert!(body.len() <= 4096, "body not capped: {} bytes", body.len());
}

#[tokio::test]
async fn non_sse_success_response_is_a_protocol_error() {
    // 例如公司 proxy 回了一個 HTML 頁面,狀態碼卻是 200。
    // body 刻意用「格式完全正確的 SSE」:如果只靠解析失敗來擋,這個請求會被當成成功。
    // 一定要是 content-type 檢查本身擋下它(mutation 檢查抓到過這個漏洞)。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_body(&hello_events()), "text/html"),
        )
        .mount(&server)
        .await;

    let err = client(&server).complete(request("hi")).await.unwrap_err();
    assert!(matches!(err, LlmError::Protocol(_)), "{err:?}");
}

#[tokio::test]
async fn stream_cut_after_output_is_interrupted_with_the_partial_response() {
    let server = MockServer::start().await;
    let mut events = hello_events();
    events.truncate(3); // 到 "Hel" 就斷了,沒有 message_stop
    Mock::given(method("POST"))
        .respond_with(sse_response(sse_body(&events)))
        .mount(&server)
        .await;

    let err = client(&server).complete(request("hi")).await.unwrap_err();
    assert!(!err.is_retryable());
    let LlmError::Interrupted { partial, cause } = err else {
        panic!("expected Interrupted, got {err:?}")
    };
    assert_eq!(
        partial.content,
        vec![Content::Text {
            text: "Hel".into(),
            meta: ProviderMetadata::default()
        }]
    );
    // 伺服器正常關閉連線,但沒有送 message_stop:原因是「串流在結束前就斷了」。
    assert!(matches!(*cause, LlmError::Protocol(_)), "{cause:?}");
}

#[tokio::test]
async fn error_event_before_any_output_is_retryable() {
    let server = MockServer::start().await;
    let events = vec![
        hello_events().remove(0),
        json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } }),
    ];
    Mock::given(method("POST"))
        .respond_with(sse_response(sse_body(&events)))
        .mount(&server)
        .await;

    let err = client(&server).complete(request("hi")).await.unwrap_err();
    assert!(matches!(err, LlmError::Http { status: 529, .. }), "{err:?}");
    assert!(err.is_retryable());
}

#[tokio::test]
async fn silent_server_hits_the_idle_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse_response(sse_body(&hello_events())).set_delay(Duration::from_secs(3)))
        .mount(&server)
        .await;

    let err = client(&server).complete(request("hi")).await.unwrap_err();
    assert!(matches!(err, LlmError::Timeout), "{err:?}");
}

#[tokio::test]
async fn invalid_requests_fail_before_anything_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let mut req = request("hi");
    req.max_tokens = 0;
    let err = client(&server).complete(req).await.unwrap_err();
    assert!(matches!(err, LlmError::InvalidRequest(_)), "{err:?}");
}

#[test]
fn api_key_never_shows_up_in_debug_output() {
    let mut config = AnthropicConfig::new(SecretString::from(KEY.to_owned()));
    config.base_url = "http://127.0.0.1:9".into();
    let client = AnthropicClient::new(config.clone()).unwrap();
    for out in [format!("{config:?}"), format!("{client:?}")] {
        assert!(!out.contains("CANARY"), "key leaked: {out}");
    }
}
