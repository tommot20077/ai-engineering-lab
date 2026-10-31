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
//! 除了 `/health`,每個請求都先通過 Host 檢查與 token 檢查(guard.rs、design-part2.md §18)。

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt, future, stream};
use llm::{JsonlWriter, LlmError, LlmProvider, Request, Response, SecretString, StreamEvent};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::ApiError;
use crate::guard;
use crate::startup::Gateway;

/// 非串流請求最多等多久(D-023)。長回答請用 `/v1/chat/stream`。
pub const CHAT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// 每個請求都拿得到的共用狀態。
// `Clone`:axum 會替每個請求複製一份。裡面全是 `Arc`,複製只是計數 +1,不複製資料。
// Java 對照:Spring 注入的 singleton bean;Rust 要明確寫出「共用」(`Arc`)。
// `pub(crate)`:同一個 crate 的 guard.rs 與測試可以直接讀寫,外面看不到。
#[derive(Clone)]
pub struct AppState {
    pub(crate) provider: Arc<dyn LlmProvider>,
    pub(crate) aliases: Arc<BTreeMap<String, String>>,
    pub(crate) records: Option<Arc<JsonlWriter>>,
    pub(crate) token: Arc<SecretString>,
    /// `Arc<[String]>`:建好之後不會再改的清單。比 `Arc<Vec<String>>` 少一層間接。
    pub(crate) allowed_hosts: Arc<[String]>,
    /// 同時呼叫上游的名額(D-023)。
    pub(crate) permits: Arc<Semaphore>,
    pub(crate) chat_timeout: Duration,
    pub(crate) max_body_bytes: usize,
}

impl From<&Gateway> for AppState {
    fn from(gateway: &Gateway) -> Self {
        let settings = &gateway.settings;
        Self {
            provider: Arc::clone(&gateway.provider),
            aliases: Arc::new(settings.aliases.clone()),
            records: gateway.records.clone(),
            token: Arc::clone(&gateway.token),
            allowed_hosts: Arc::from(settings.allowed_hosts.clone()),
            permits: Arc::new(Semaphore::new(settings.max_concurrent)),
            chat_timeout: CHAT_TIMEOUT,
            max_body_bytes: settings.max_body_bytes,
        }
    }
}

/// 每隔多久送一行 SSE 註解 `: keep-alive`。長時間 thinking 時可能幾十秒沒有事件,
/// 中間的 proxy 看到連線太安靜會把它切掉(D-023)。
const KEEP_ALIVE: Duration = Duration::from_secs(15);

pub fn router(state: AppState) -> Router {
    // Java 對照:像是把多個 `@GetMapping` / `@PostMapping` 寫在同一個地方;
    // `.with_state` 等於把共用的 bean 注入給所有 handler。
    let protected = Router::new()
        .route("/v1/status", get(status))
        .route("/v1/models", get(models))
        .route("/v1/chat", post(chat))
        .route("/v1/chat/stream", post(chat_stream))
        // 沒有對應的路徑:回我們統一格式的 404,而不是 axum 預設的空 body。
        .fallback(|| async { ApiError::not_found() })
        // `layer` 一層包一層,跟 decorator 一樣:「後加的在外面」。
        // 所以請求的順序是:Host 檢查 → token 檢查 → body 上限 → handler(design-part2.md §18)。
        // body 上限:超過時 `Json` 解析失敗(413),經由 error.rs 轉成 `payload_too_large`。
        .layer(DefaultBodyLimit::max(state.max_body_bytes))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            guard::require_token,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            guard::require_allowed_host,
        ));

    // `/health` 放在受保護的 router 外面:服務檢查帶不了 token,也可能用 IP 當 Host(D-025)。
    // 注意:axum 的 `.layer()` 只包住「在它之前加入」的路由。順序寫錯,保護範圍就跟著變。
    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

/// 拿一個呼叫上游的名額。拿不到就立刻回 429,不排隊(D-023)。
// `try_acquire_owned`:不等待;`owned` 的 permit 不借用 Semaphore(它自己持有一份 `Arc`),
// 所以可以被搬進串流、活得比這個函式久。permit 被丟掉時,名額自動歸還(又是 Drop)。
// Java 對照:`semaphore.tryAcquire()`,但歸還不用寫 finally。
fn acquire(state: &AppState) -> Result<OwnedSemaphorePermit, ApiError> {
    Arc::clone(&state.permits)
        .try_acquire_owned()
        .map_err(|_| ApiError {
            retryable: true,
            ..ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "gateway_busy",
                "too many requests in flight; try again shortly",
            )
        })
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
    // `_permit`(有名字):活到函式結束,回應組好才歸還名額。寫成 `_` 的話會立刻被丟掉。
    let _permit = acquire(&state)?;
    // 兩個 `?`:外層的 `timeout` 可能逾時(第一個 `?`,轉成 504);
    // 沒逾時的話,裡面的結果是 `Result<Response, LlmError>`(第二個 `?`,轉成對應的狀態碼)。
    // 逾時時 `complete()` 的 future 被丟掉 → 上游請求被取消,Record 記成 cancelled。
    let response = tokio::time::timeout(state.chat_timeout, state.provider.complete(request))
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                format!(
                    "no complete answer within {} s; use /v1/chat/stream for long answers",
                    state.chat_timeout.as_secs()
                ),
            )
        })??;
    Ok(Json(response))
}

/// `POST /v1/chat/stream`:SSE。
async fn chat_stream(
    State(state): State<AppState>,
    body: Result<Json<Request>, JsonRejection>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let request = parse(body)?;
    let permit = acquire(&state)?;
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
        // `move` 把 permit 搬進 closure,closure 又被串流持有:串流活多久,名額就佔多久。
        // 呼叫者斷線 → 串流被丟掉 → permit 被丟掉 → 名額歸還。
        // closure 裡一定要「用到」permit,`move` 才會把它搬進來;只是拿個參考,不做任何事。
        .map(move |item| {
            let _held = &permit;
            Ok(to_sse_event(item))
        });
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

/// `GET /v1/status`:給自己除錯用,需要 token。
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
    use axum::http::header;
    use llm::StopKind;
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;
    use crate::testing::{
        HOST, Scripted, TOKEN, answer, chat_body, delta, request, send, start, state, text_start,
        within,
    };

    fn app(script: crate::testing::Script) -> (Router, Arc<Scripted>) {
        let provider = Scripted::new(script);
        (router(state(provider.clone())), provider)
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
            LlmError::http(429, Some(Duration::from_secs(7)), "")
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
        assert_eq!(provider.calls(), 0);
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
        assert_eq!(provider.calls(), 0);
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

    #[tokio::test]
    async fn a_body_over_the_limit_is_413_without_calling_the_provider() {
        let provider = Scripted::new(answer());
        let app = router(AppState {
            max_body_bytes: 64,
            ..state(provider.clone())
        });

        let reply = send(app, "POST", "/v1/chat", Some(chat_body(64))).await;

        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(reply.json()["error"]["kind"], "payload_too_large");
        assert_eq!(provider.calls(), 0);
    }

    #[tokio::test]
    async fn a_stream_holds_its_permit_until_it_ends() {
        // 只有 1 個名額;回答一直沒結束(hanging)。
        let provider = Scripted::hanging(vec![Ok(start()), Ok(text_start()), Ok(delta("你"))]);
        let app = router(AppState {
            permits: Arc::new(Semaphore::new(1)),
            ..state(provider.clone())
        });

        // 第一個串流:拿到回應(200)但不讀完 body,串流還活著 → 名額還被佔著。
        let first = app
            .clone()
            .oneshot(request(
                "POST",
                "/v1/chat/stream",
                Some(chat_body(64)),
                Some(HOST),
                Some(TOKEN),
            ))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let busy = within(send(
            app.clone(),
            "POST",
            "/v1/chat/stream",
            Some(chat_body(64)),
        ))
        .await;
        assert_eq!(busy.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(busy.json()["error"]["kind"], "gateway_busy");
        assert_eq!(busy.json()["error"]["retryable"], true);
        assert_eq!(provider.calls(), 1, "滿了就不該再呼叫上游");

        // 呼叫者斷線(丟掉回應)→ 串流被丟掉 → 名額歸還。
        drop(first);
        let again = app
            .oneshot(request(
                "POST",
                "/v1/chat/stream",
                Some(chat_body(64)),
                Some(HOST),
                Some(TOKEN),
            ))
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn chat_permits_are_returned_after_each_response() {
        let provider = Scripted::new(answer());
        let app = router(AppState {
            permits: Arc::new(Semaphore::new(1)),
            ..state(provider)
        });

        for _ in 0..3 {
            let reply = send(app.clone(), "POST", "/v1/chat", Some(chat_body(64))).await;
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn chat_that_never_finishes_times_out_with_504() {
        let provider = Scripted::hanging(vec![Ok(start())]);
        let app = router(AppState {
            chat_timeout: Duration::from_secs(1),
            ..state(provider)
        });

        let reply = within(send(app, "POST", "/v1/chat", Some(chat_body(64)))).await;

        assert_eq!(reply.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(reply.json()["error"]["kind"], "upstream_timeout");
    }
}
