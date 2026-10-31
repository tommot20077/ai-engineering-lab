//! HTTP 端點(design-part2.md §17、D-023)。
//!
//! | 端點 | 回應 |
//! |---|---|
//! | `POST /v1/chat` | 完整的 `Response`(JSON) |
//! | `POST /v1/chat/stream` | SSE:`event: <type>` + `data: <StreamEvent JSON>` |
//! | `GET /v1/models` | 別名清單 |
//! | `GET /v1/status` | 版本、紀錄遺失數 |
//! | `GET /health` | `{"status":"ok"}`,給服務檢查用 |
//!
//! token、Host 檢查、body 上限、並行上限在第 17 步加上(design-part2.md §18)。

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt, future, stream};
use llm::{JsonlWriter, LlmError, LlmProvider, Request, Response, StreamEvent};
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::startup::Gateway;

/// 每個請求都拿得到的共用狀態。
// `Clone`:axum 會替每個請求複製一份。裡面全是 `Arc`,複製只是計數 +1,不複製資料。
// Java 對照:Spring 注入的 singleton bean;Rust 要明確寫出「共用」(`Arc`)。
#[derive(Clone)]
pub struct AppState {
    provider: Arc<dyn LlmProvider>,
    aliases: Arc<BTreeMap<String, String>>,
    records: Option<Arc<JsonlWriter>>,
}

impl AppState {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        aliases: BTreeMap<String, String>,
        records: Option<Arc<JsonlWriter>>,
    ) -> Self {
        Self {
            provider,
            aliases: Arc::new(aliases),
            records,
        }
    }
}

impl From<&Gateway> for AppState {
    fn from(gateway: &Gateway) -> Self {
        Self::new(
            Arc::clone(&gateway.provider),
            gateway.settings.aliases.clone(),
            gateway.records.clone(),
        )
    }
}

/// 每隔多久送一行 SSE 註解 `: keep-alive`。長時間 thinking 時可能幾十秒沒有事件,
/// 中間的 proxy 看到連線太安靜會把它切掉(D-023)。
const KEEP_ALIVE: Duration = Duration::from_secs(15);

pub fn router(state: AppState) -> Router {
    // Java 對照:像是把多個 `@GetMapping` / `@PostMapping` 寫在同一個地方;
    // `.with_state` 等於把共用的 bean 注入給所有 handler。
    Router::new()
        .route("/health", get(health))
        .route("/v1/status", get(status))
        .route("/v1/models", get(models))
        .route("/v1/chat", post(chat))
        .route("/v1/chat/stream", post(chat_stream))
        // 沒有對應的路徑:回我們統一格式的 404,而不是 axum 預設的空 body。
        .fallback(|| async { ApiError::not_found() })
        .with_state(state)
}

/// 讀出並檢查請求 body。
// 參數型別是 `Result<Json<Request>, JsonRejection>`:axum 解析失敗時不會自己回純文字錯誤,
// 而是把失敗交給我們,轉成統一的 JSON 格式。
fn parse(body: Result<Json<Request>, JsonRejection>) -> Result<Request, ApiError> {
    let Json(request) = body?;
    // 送出前先自我檢查(例如 max_tokens = 0):不合法的請求不碰上游,也不浪費額度。
    request.validate()?;
    Ok(request)
}

/// `POST /v1/chat`:等回答完,回傳完整的 `Response`。
// handler 就是一個 async 函式:參數是「要從請求裡取出什麼」(extractor),回傳值是「怎麼變成回應」。
// `State(state)`:模式比對直接把 extractor 拆開。Java 對照:`@RequestBody`、注入的欄位。
async fn chat(
    State(state): State<AppState>,
    body: Result<Json<Request>, JsonRejection>,
) -> Result<Json<Response>, ApiError> {
    let request = parse(body)?;
    // `?` 把 `LlmError` 轉成 `ApiError`(靠 error.rs 的 `From<LlmError>`)。
    let response = state.provider.complete(request).await?;
    Ok(Json(response))
}

/// `POST /v1/chat/stream`:SSE。
async fn chat_stream(
    State(state): State<AppState>,
    body: Result<Json<Request>, JsonRejection>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let request = parse(body)?;
    let mut events = state.provider.stream(request);

    // 先等第一個項目,再決定狀態碼(D-023):HTTP 狀態碼一送出就改不了。
    // 是錯誤 → 還來得及回 429 / 502 這類正確的狀態碼;是事件 → 回 200,開始 SSE。
    // 這不會讓使用者多等:Retry 本來就扣住開始事件,第一個項目出現時就是第一個字出現時。
    let first = match events.next().await {
        Some(Ok(event)) => event,
        Some(Err(error)) => return Err(error.into()),
        None => return Err(LlmError::protocol("stream ended before any event").into()),
    };

    // 把第一個事件接回串流最前面,其餘照原樣轉成 SSE 事件。
    // 呼叫者斷線時 axum 會丟掉這個串流 → provider 的串流也被丟掉 → 上游連線關閉,Record 記成 cancelled。
    let sse = stream::once(future::ready(Ok(first)))
        .chain(events)
        .map(|item| Ok(to_sse_event(item)));
    Ok(Sse::new(sse).keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text("keep-alive")))
}

/// 一個串流項目 → 一個 SSE 事件。
fn to_sse_event(item: llm::Result<StreamEvent>) -> Event {
    match item {
        // `event:` 是事件種類,`data:` 是 `StreamEvent` 原本的 JSON(自帶 type)。
        Ok(event) => Event::default()
            .event(event.type_name())
            .json_data(&event)
            // 序列化 StreamEvent 不會失敗;萬一失敗,也送一個合法的錯誤事件,而不是讓串流壞掉。
            .unwrap_or_else(|e| error_event(&ApiError::from(LlmError::protocol(e.to_string())))),
        // 已經送出 200 了,錯誤只能是串流裡的一個事件;格式跟 JSON 錯誤一樣。之後串流就結束。
        Err(error) => error_event(&ApiError::from(error)),
    }
}

fn error_event(error: &ApiError) -> Event {
    Event::default().event("error").data(error.to_json())
}

/// `GET /v1/models`:可以用的別名(依字母順序)。
async fn models(State(state): State<AppState>) -> Json<Value> {
    let models: Vec<Value> = state
        .aliases
        .iter()
        .map(|(alias, model)| json!({ "alias": alias, "model": model }))
        .collect();
    Json(json!({ "models": models }))
}

/// `GET /v1/status`:給自己除錯用。第 17 步起需要 token。
async fn status(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        // `env!`:編譯時就把 Cargo.toml 的版本號寫進程式。
        "version": env!("CARGO_PKG_VERSION"),
        "aliases": state.aliases.len(),
        "records": {
            "enabled": state.records.is_some(),
            "dropped": state.records.as_ref().map_or(0, |w| w.dropped()),
        },
    }))
}

/// `GET /health`:只說「還活著」,不回傳任何資訊(D-025)。
async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode, header};
    use llm::{Content, EventStream, Message, ProviderMetadata, Role, StopKind, StopReason, Usage};
    use tower::ServiceExt;

    use super::*;

    type Script = Vec<std::result::Result<StreamEvent, fn() -> LlmError>>;

    /// 假的 provider:照劇本吐事件,並計算被呼叫了幾次。
    struct Scripted {
        script: Script,
        calls: AtomicUsize,
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
            Box::pin(stream::iter(items))
        }
    }

    fn start() -> StreamEvent {
        StreamEvent::MessageStart {
            id: Some("msg_1".into()),
            model: "claude-haiku-5-5".into(),
        }
    }

    fn text_start() -> StreamEvent {
        StreamEvent::TextStart {
            index: 0,
            meta: ProviderMetadata::default(),
        }
    }

    fn delta(text: &str) -> StreamEvent {
        StreamEvent::TextDelta {
            index: 0,
            text: text.into(),
        }
    }

    fn answer() -> Script {
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

    fn app(script: Script) -> (Router, Arc<Scripted>) {
        let provider = Arc::new(Scripted {
            script,
            calls: AtomicUsize::new(0),
        });
        let aliases = BTreeMap::from([
            ("default".into(), "claude-sonnet-5-5".into()),
            ("cheap".into(), "claude-haiku-5-5".into()),
        ]);
        let state = AppState::new(provider.clone(), aliases, None);
        (router(state), provider)
    }

    fn chat_body(max_tokens: u32) -> String {
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

    struct Reply {
        status: StatusCode,
        headers: axum::http::HeaderMap,
        body: String,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
        }

        /// SSE 裡依序出現的 `event:` 名稱。
        fn event_names(&self) -> Vec<&str> {
            self.body
                .lines()
                .filter_map(|line| line.strip_prefix("event: "))
                .collect()
        }
    }

    /// 不開 port:用 tower 的 `oneshot` 把一個請求直接丟給 router。
    async fn send(app: Router, method: &str, uri: &str, body: Option<String>) -> Reply {
        let mut request = HttpRequest::builder().method(method).uri(uri);
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let request = request
            .body(body.map(Body::from).unwrap_or_else(Body::empty))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
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

    #[tokio::test]
    async fn chat_returns_the_whole_response_as_json() {
        let (app, _) = app(answer());

        let reply = send(app, "POST", "/v1/chat", Some(chat_body(64))).await;

        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let response: Response = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(response.model, "claude-haiku-5-5");
        assert_eq!(response.stop.kind, StopKind::EndTurn);
    }

    #[tokio::test]
    async fn stream_sends_one_sse_event_per_stream_event() {
        let (app, _) = app(answer());

        let reply = send(app, "POST", "/v1/chat/stream", Some(chat_body(64))).await;

        assert_eq!(reply.status, StatusCode::OK);
        assert!(
            reply.headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        assert_eq!(
            reply.event_names(),
            [
                "message_start",
                "text_start",
                "text_delta",
                "text_end",
                "finish"
            ]
        );
        // data 就是 StreamEvent 原本的 JSON(自帶 type):呼叫者可以用 llm 的型別直接讀回來。
        assert!(
            reply
                .body
                .contains(r#"data: {"type":"text_delta","index":0,"text":"你好"}"#),
            "{}",
            reply.body
        );
    }

    #[tokio::test]
    async fn stream_error_before_any_event_gets_a_real_status_code() {
        // 先等第一個項目再決定狀態碼:這時還沒送出 200,所以能回 429 + JSON(D-023)。
        let (app, _) = app(vec![Err(|| {
            LlmError::http(429, Some(std::time::Duration::from_secs(7)), "")
        })]);

        let reply = send(app, "POST", "/v1/chat/stream", Some(chat_body(64))).await;

        assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(reply.headers[header::RETRY_AFTER], "7");
        assert_eq!(reply.json()["error"]["kind"], "rate_limited");
    }

    #[tokio::test]
    async fn stream_error_after_output_becomes_an_error_event() {
        let (app, _) = app(vec![
            Ok(start()),
            Ok(text_start()),
            Ok(delta("你")),
            Err(|| LlmError::Transport("reset".into())),
        ]);

        let reply = send(app, "POST", "/v1/chat/stream", Some(chat_body(64))).await;

        // 200 已經送出去了,錯誤只能是串流裡的一個事件,格式跟 JSON 錯誤一樣。
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.event_names(),
            ["message_start", "text_start", "text_delta", "error"]
        );
        assert!(
            reply
                .body
                .contains(r#"data: {"error":{"kind":"upstream_error""#),
            "{}",
            reply.body
        );
    }

    #[tokio::test]
    async fn malformed_json_gets_our_error_format_without_calling_the_provider() {
        let (app, provider) = app(answer());

        let reply = send(app, "POST", "/v1/chat", Some("{not json".into())).await;

        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error"]["kind"], "invalid_request");
        assert_eq!(provider.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn an_invalid_request_is_rejected_before_calling_the_provider() {
        let (app, provider) = app(answer());

        // max_tokens = 0:能解析,但 `Request::validate()` 不會過。
        let reply = send(app, "POST", "/v1/chat/stream", Some(chat_body(0))).await;

        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(
            reply.json()["error"]["message"]
                .as_str()
                .unwrap()
                .contains("max_tokens"),
            "{}",
            reply.body
        );
        assert_eq!(provider.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn interrupted_chat_returns_the_partial_answer() {
        let (app, _) = app(vec![
            Ok(start()),
            Ok(text_start()),
            Ok(delta("你")),
            Err(|| LlmError::Transport("reset".into())),
        ]);

        let reply = send(app, "POST", "/v1/chat", Some(chat_body(64))).await;

        assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
        let error = &reply.json()["error"];
        assert_eq!(error["kind"], "interrupted");
        assert_eq!(error["partial"]["content"][0]["text"], "你");
    }

    #[tokio::test]
    async fn models_lists_the_aliases() {
        let (app, _) = app(answer());

        let reply = send(app, "GET", "/v1/models", None).await;

        assert_eq!(
            reply.json(),
            json!({ "models": [
                { "alias": "cheap", "model": "claude-haiku-5-5" },
                { "alias": "default", "model": "claude-sonnet-5-5" }
            ]})
        );
    }

    #[tokio::test]
    async fn status_and_health() {
        let (app, _) = app(answer());

        let status = send(app.clone(), "GET", "/v1/status", None).await;
        let health = send(app, "GET", "/health", None).await;

        assert_eq!(
            status.json(),
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "aliases": 2,
                "records": { "enabled": false, "dropped": 0 }
            })
        );
        assert_eq!(health.json(), json!({ "status": "ok" }));
    }

    #[tokio::test]
    async fn unknown_paths_get_a_json_404() {
        let (app, _) = app(answer());

        let reply = send(app, "GET", "/v1/nope", None).await;

        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.json()["error"]["kind"], "not_found");
    }
}
