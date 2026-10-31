//! 測試共用的工具:假的 provider、組 AppState、送請求(只在 `cargo test` 時編譯)。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request as HttpRequest, StatusCode, header};
use futures::{StreamExt, stream};
use llm::{
    Content, EventStream, LlmError, LlmProvider, Message, ProviderMetadata, Request, Role,
    SecretString, StopKind, StopReason, StreamEvent, Usage,
};
use serde_json::Value;
use tokio::sync::Semaphore;
use tower::ServiceExt;

use crate::api::{AppState, CHAT_TIMEOUT};
use crate::config::DEFAULT_MAX_BODY_BYTES;

pub(crate) const TOKEN: &str = "test-token";
pub(crate) const HOST: &str = "127.0.0.1:8787";

pub(crate) type Script = Vec<std::result::Result<StreamEvent, fn() -> LlmError>>;

/// 假的 provider:照劇本吐事件,並計算被呼叫了幾次。
pub(crate) struct Scripted {
    script: Script,
    /// 劇本吐完之後不結束,一直等下去:模擬「回答很長、還沒完」。
    hang: bool,
    calls: AtomicUsize,
}

impl Scripted {
    pub(crate) fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            hang: false,
            calls: AtomicUsize::new(0),
        })
    }

    pub(crate) fn hanging(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            hang: true,
            calls: AtomicUsize::new(0),
        })
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl LlmProvider for Scripted {
    fn stream(&self, _request: Request) -> EventStream {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let items: Vec<llm::Result<StreamEvent>> = self
            .script
            .iter()
            .map(|item| match item {
                Ok(event) => Ok(event.clone()),
                Err(make_error) => Err(make_error()),
            })
            .collect();
        let items = stream::iter(items);
        if self.hang {
            Box::pin(items.chain(stream::pending()))
        } else {
            Box::pin(items)
        }
    }
}

/// 一個「什麼都打開」的 AppState:別名 default / cheap、不記錄、並行上限與逾時用預設值。
pub(crate) fn state(provider: Arc<dyn LlmProvider>) -> AppState {
    AppState {
        provider,
        aliases: Arc::new(BTreeMap::from([
            ("default".into(), "claude-sonnet-5-5".into()),
            ("cheap".into(), "claude-haiku-5-5".into()),
        ])),
        records: None,
        token: Arc::new(SecretString::from(TOKEN)),
        allowed_hosts: Arc::from(vec![HOST.to_owned(), "localhost:8787".to_owned()]),
        permits: Arc::new(Semaphore::new(16)),
        chat_timeout: CHAT_TIMEOUT,
        max_body_bytes: DEFAULT_MAX_BODY_BYTES,
    }
}

pub(crate) fn start() -> StreamEvent {
    StreamEvent::MessageStart {
        id: Some("msg_1".into()),
        model: "claude-haiku-5-5".into(),
    }
}

pub(crate) fn text_start() -> StreamEvent {
    StreamEvent::TextStart {
        index: 0,
        meta: ProviderMetadata::default(),
    }
}

pub(crate) fn delta(text: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        index: 0,
        text: text.into(),
    }
}

pub(crate) fn answer() -> Script {
    vec![
        Ok(start()),
        Ok(text_start()),
        Ok(delta("你好")),
        Ok(StreamEvent::TextEnd {
            index: 0,
            meta: ProviderMetadata::default(),
        }),
        Ok(StreamEvent::Finish {
            stop: StopReason {
                kind: StopKind::EndTurn,
                raw: "end_turn".into(),
            },
            usage: Usage::default(),
            meta: ProviderMetadata::default(),
        }),
    ]
}

pub(crate) fn chat_body(max_tokens: u32) -> String {
    serde_json::to_string(&Request {
        model: "cheap".into(),
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "hi".into(),
                meta: ProviderMetadata::default(),
            }],
        }],
        tools: vec![],
        max_tokens,
        provider_options: ProviderMetadata::default(),
    })
    .unwrap()
}

pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: String,
}

impl Reply {
    pub(crate) fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    /// SSE 裡依序出現的 `event:` 名稱。
    pub(crate) fn event_names(&self) -> Vec<&str> {
        self.body
            .lines()
            .filter_map(|line| line.strip_prefix("event: "))
            .collect()
    }
}

/// 可能卡住的呼叫加上保險:實作錯的時候,測試要「失敗」,而不是永遠跑不完。
/// 在暫停時間的測試裡,這 5 秒會被自動快轉,不會真的等。
pub(crate) async fn within<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(std::time::Duration::from_secs(5), future)
        .await
        .expect("hung: the request never finished")
}

/// 帶著正確的 Host 與 token 送出請求。
pub(crate) async fn send(app: Router, method: &str, uri: &str, body: Option<String>) -> Reply {
    send_as(app, method, uri, body, Some(HOST), Some(TOKEN)).await
}

/// 不開 port:用 tower 的 `oneshot` 把一個請求直接丟給 router。`None` = 不帶那個 header。
pub(crate) async fn send_as(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<String>,
    host: Option<&str>,
    token: Option<&str>,
) -> Reply {
    let response = app
        .oneshot(request(method, uri, body, host, token))
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

pub(crate) fn request(
    method: &str,
    uri: &str,
    body: Option<String>,
    host: Option<&str>,
    token: Option<&str>,
) -> HttpRequest<Body> {
    let mut request = HttpRequest::builder().method(method).uri(uri);
    if let Some(host) = host {
        request = request.header(header::HOST, host);
    }
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    request
        .body(body.map(Body::from).unwrap_or_else(Body::empty))
        .unwrap()
}
