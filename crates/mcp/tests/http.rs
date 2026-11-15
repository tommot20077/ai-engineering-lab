//! Streamable HTTP 傳輸專屬的整合測試:token、redirect、SSE、上限、狀態碼(design-part2 §5)。
//! 跟 stdio 共通的行為在 `tests/client.rs`(兩種傳輸各跑一次)。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{http_server, within};
use mcp::jsonrpc::RpcError;
use mcp::transport::{HttpConfig, HttpLimits, HttpTransport, Transport};
use mcp::{CallResult, ClientOptions, McpClient, McpError};
use serde_json::{Map, Value, json};
use tokio::process::Child;

fn transport(url: &str, token: Option<&str>, limits: HttpLimits) -> Box<HttpTransport> {
    let config = HttpConfig {
        url: url.into(),
        token: token.map(|t| t.to_owned().into()),
    };
    Box::new(HttpTransport::new(&config, limits).unwrap())
}

async fn connect(
    flags: &[&str],
    token: Option<&str>,
    limits: HttpLimits,
) -> (Result<McpClient, McpError>, Child) {
    let (url, server) = http_server(flags).await;
    let result = within(McpClient::connect(
        transport(&url, token, limits),
        ClientOptions::default(),
    ))
    .await;
    (result, server)
}

async fn echo(client: &McpClient, text: &str) -> Result<CallResult, McpError> {
    let mut arguments = Map::new();
    arguments.insert("text".into(), json!(text));
    within(client.call_tool("echo", arguments)).await
}

async fn big(client: &McpClient, bytes: usize) -> Result<CallResult, McpError> {
    let mut arguments = Map::new();
    arguments.insert("bytes".into(), json!(bytes));
    within(client.call_tool("big", arguments)).await
}

fn ok(text: &str) -> Result<CallResult, McpError> {
    Ok(CallResult {
        text: text.into(),
        is_error: false,
    })
}

// ---- 認證 ----

#[tokio::test]
async fn the_bearer_token_is_sent() {
    let (client, _server) = connect(
        &["--token", "tok-123"],
        Some("tok-123"),
        HttpLimits::default(),
    )
    .await;
    assert_eq!(echo(&client.unwrap(), "hi").await, ok("hi"));
}

#[tokio::test]
async fn a_missing_token_is_an_http_error() {
    let (client, _server) = connect(&["--token", "tok-123"], None, HttpLimits::default()).await;
    assert_eq!(
        client.err(),
        Some(McpError::Http {
            status: 401,
            body: "unauthorized".into()
        })
    );
}

#[tokio::test]
async fn redirects_are_not_followed() {
    // 跟過去的話,token 會被帶到 redirect 指的地方。我們不跟:307 本身就是錯誤,目的地沒有人來過。
    let (url, _server) = http_server(&["--redirect"]).await;
    let result = within(McpClient::connect(
        transport(&url, Some("tok-123"), HttpLimits::default()),
        ClientOptions::default(),
    ))
    .await;
    assert_eq!(
        result.err(),
        Some(McpError::Http {
            status: 307,
            body: String::new()
        })
    );
    let hits_url = url.replace("/mcp", "/hits");
    let hits = reqwest::get(&hits_url).await.unwrap().text().await.unwrap();
    assert_eq!(hits, "0");
}

// ---- SSE 回應 ----

#[tokio::test]
async fn an_sse_response_skips_notifications_and_comments() {
    // server 先送一行註解、一個 progress 通知,回應的 JSON 拆成好幾行 `data:`。
    let (client, _server) = connect(&["--sse"], None, HttpLimits::default()).await;
    let client = client.unwrap();
    assert_eq!(within(client.list_tools()).await.unwrap().tools.len(), 12);
    assert_eq!(echo(&client, "streamed").await, ok("streamed"));
}

#[tokio::test]
async fn a_request_on_the_response_stream_is_a_protocol_error() {
    let (url, _server) = http_server(&["--sse", "--sse-request"]).await;
    let t = transport(&url, None, HttpLimits::default());
    assert_eq!(
        within(t.request("tools/list", Some(meta()))).await,
        Err(McpError::Protocol(
            "the server sent a request on a response stream".into()
        ))
    );
}

#[tokio::test]
async fn a_stream_that_ends_without_a_response_is_a_protocol_error() {
    let (url, _server) = http_server(&["--sse", "--sse-truncate"]).await;
    let t = transport(&url, None, HttpLimits::default());
    assert_eq!(
        within(t.request("tools/list", Some(meta()))).await,
        Err(McpError::Protocol(
            "the event stream ended without a response".into()
        ))
    );
}

#[tokio::test]
async fn a_response_that_is_neither_json_nor_sse_is_a_protocol_error() {
    let (url, _server) = http_server(&["--text-response"]).await;
    let t = transport(&url, None, HttpLimits::default());
    assert_eq!(
        within(t.request("tools/list", Some(meta()))).await,
        Err(McpError::Protocol(
            "expected application/json or text/event-stream, got HTTP 200 with text/plain".into()
        ))
    );
}

// ---- 上限 ----

#[tokio::test]
async fn a_json_body_over_the_limit_is_too_large() {
    let limits = HttpLimits {
        max_message_bytes: 1000,
        ..HttpLimits::default()
    };
    let (client, _server) = connect(&[], None, limits).await;
    let client = client.unwrap();
    assert_eq!(big(&client, 500).await.unwrap().text.len(), 500);
    assert_eq!(
        big(&client, 2000).await,
        Err(McpError::TooLarge { limit: 1000 })
    );
    // HTTP 的每個請求是獨立的:太大的那一個失敗,之後照常。
    assert_eq!(echo(&client, "after").await, ok("after"));
}

#[tokio::test]
async fn an_sse_event_over_the_limit_is_a_protocol_error() {
    let limits = HttpLimits {
        max_message_bytes: 1000,
        ..HttpLimits::default()
    };
    let (client, _server) = connect(&["--sse"], None, limits).await;
    let client = client.unwrap();
    assert_eq!(big(&client, 500).await.unwrap().text.len(), 500);
    assert!(matches!(
        big(&client, 2000).await,
        Err(McpError::Protocol(_))
    ));
}

#[tokio::test]
async fn a_whole_stream_over_the_limit_is_too_large() {
    let limits = HttpLimits {
        max_stream_bytes: 1000,
        ..HttpLimits::default()
    };
    let (client, _server) = connect(&["--sse"], None, limits).await;
    let client = client.unwrap();
    assert_eq!(
        big(&client, 2000).await,
        Err(McpError::TooLarge { limit: 1000 })
    );
}

// ---- 狀態碼與通知 ----

/// 只有 `_meta` 的參數(直接用傳輸送請求時用)。
fn meta() -> Value {
    json!({"_meta": {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    }})
}

#[tokio::test]
async fn an_unknown_method_is_a_404_with_a_json_rpc_error() {
    let (url, _server) = http_server(&[]).await;
    let t = transport(&url, None, HttpLimits::default());
    assert_eq!(
        within(t.request("no/such/method", Some(meta()))).await,
        Err(McpError::Rpc(RpcError {
            code: -32601,
            message: "Method not found".into(),
            data: None
        }))
    );
}

#[tokio::test]
async fn a_notification_is_accepted_with_202() {
    let (url, _server) = http_server(&[]).await;
    let t = transport(&url, None, HttpLimits::default());
    assert_eq!(
        within(t.notify("notifications/initialized", None)).await,
        Ok(())
    );
}

#[tokio::test]
async fn a_server_that_is_not_there_is_a_network_error_without_the_url() {
    // port 1 沒有人聽。錯誤訊息不帶 URL(URL 可能含有不該進 log 的東西)。
    let t = transport("http://127.0.0.1:1/mcp", None, HttpLimits::default());
    let err = within(t.request("tools/list", Some(meta())))
        .await
        .unwrap_err();
    let McpError::Network(message) = &err else {
        panic!("{err:?}")
    };
    assert!(!message.contains("127.0.0.1"), "{message}");
}

#[tokio::test]
async fn a_bad_url_is_refused_before_any_request() {
    let config = HttpConfig {
        url: "http://example.com/mcp".into(),
        token: None,
    };
    assert!(matches!(
        HttpTransport::new(&config, HttpLimits::default()),
        Err(McpError::InvalidUrl(_))
    ));
}
