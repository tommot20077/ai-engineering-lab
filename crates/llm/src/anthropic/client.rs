//! 真正打 HTTP 的那一層:把翻譯好的 body 送出去,再把回來的 SSE 串流接到翻譯器上。
//!
//! 一次請求只打一次,不自己重試(design.md §6);重試是之後 middleware 的事。

use std::time::Duration;

use async_stream::try_stream;
use futures::StreamExt;
use futures::future::BoxFuture;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use secrecy::{ExposeSecret, SecretString};

use super::translate::{EventTranslator, request_body};
use crate::error::{LlmError, MAX_ERROR_BODY_BYTES, Result};
use crate::limits::Limits;
use crate::provider::{EventStream, LlmProvider, collect};
use crate::sse::SseParser;
use crate::types::{Request, Response};

/// Anthropic API 的預設位址。
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2026-10-21";
/// 請求 body 預設帶 `fallbacks`(translate.rs),需要這個 beta header。
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// 建立 `AnthropicClient` 需要的設定。
///
/// 用 `AnthropicConfig::new(key)` 取得預設值,再改想調整的欄位。
// `Debug` 可以放心 derive:`SecretString` 的 Debug 只會印 `[REDACTED]`。
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: SecretString,
    pub base_url: String,
    /// 建立連線最多等多久。
    pub connect_timeout: Duration,
    /// 兩次收到資料之間最多等多久。不設「整個請求的總時間」:長回答串流幾分鐘是正常的,
    /// 真正要擋的是「伺服器不說話了」。
    pub idle_timeout: Duration,
    pub limits: Limits,
}

impl AnthropicConfig {
    // library 不自己讀環境變數(design.md §7):key 從哪裡來,由呼叫者決定。
    pub fn new(api_key: SecretString) -> Self {
        Self {
            api_key,
            base_url: DEFAULT_BASE_URL.into(),
            connect_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(120),
            limits: Limits::default(),
        }
    }
}

/// Anthropic Messages API 的 provider。
// `Clone` 很便宜:`reqwest::Client` 內部本來就是 `Arc`,複製只是多一個參考,連線池是共用的。
#[derive(Debug, Clone)]
pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: SecretString,
    messages_url: String,
    limits: Limits,
}

impl AnthropicClient {
    pub fn new(config: AnthropicConfig) -> Result<Self> {
        // 整個 client 共用一個 `reqwest::Client`:連線池重複使用,不必每次重新握手(design.md §8)。
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.idle_timeout)
            .build()
            .map_err(transport)?;
        Ok(Self {
            http,
            api_key: config.api_key,
            messages_url: format!("{}/v1/messages", config.base_url.trim_end_matches('/')),
            limits: config.limits,
        })
    }

    /// 檢查並組好請求。全部是同步的工作,失敗就不必碰網路。
    fn prepare(&self, request: &Request) -> Result<reqwest::RequestBuilder> {
        request.validate()?;
        let body = serde_json::to_vec(&request_body(request)?)
            .map_err(|e| LlmError::InvalidRequest(format!("cannot encode request: {e}")))?;
        // 整個專案唯一碰到原始 key 的地方。搜尋 `expose_secret` 就能找到它。
        let mut key = HeaderValue::from_str(self.api_key.expose_secret())
            .map_err(|_| LlmError::InvalidRequest("API key is not a valid header value".into()))?;
        // 標記成敏感:之後就算有人把整份 header 印出來,這個值也會被遮住。
        key.set_sensitive(true);
        Ok(self
            .http
            .post(&self.messages_url)
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", FALLBACK_BETA)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .body(body))
    }
}

impl LlmProvider for AnthropicClient {
    fn stream(&self, request: Request) -> EventStream {
        // 先在這裡(同步)把 body 組好:之後的 stream 只擁有組好的請求,不再借用 `request` 或 `self`,
        // 所以它可以是 `'static`,被搬到別的執行緒、活得比這次呼叫久。
        let prepared = self.prepare(&request);
        let limits = self.limits;
        // `try_stream!`:在裡面用 `yield` 吐出事件、用 `?` 吐出錯誤(錯誤吐出後串流就結束)。
        // Java 對照:很像用 generator 寫一個 Iterator,只是每一步都可以 `.await`。
        Box::pin(try_stream! {
            let response = prepared?.send().await.map_err(transport)?;
            let response = check_response(response).await?;
            let mut parser = SseParser::with_max_event_bytes(limits.max_event_bytes);
            let mut translator = EventTranslator::new();
            let mut body = response.bytes_stream();
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(transport)?;
                for sse in parser.feed(&chunk)? {
                    for event in translator.translate(&sse)? {
                        yield event;
                    }
                }
            }
        })
    }

    // 覆寫預設方法:用這個 client 自己的上限,而不是預設值。
    fn complete(&self, request: Request) -> BoxFuture<'_, Result<Response>> {
        Box::pin(collect(self.stream(request), self.limits))
    }
}

/// 非 2xx 轉成錯誤;2xx 但不是 SSE(例如 proxy 回了 HTML 頁面)也當成錯誤。
async fn check_response(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if !status.is_success() {
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = read_capped(response, MAX_ERROR_BODY_BYTES).await;
        return Err(LlmError::http(status.as_u16(), retry_after, &body));
    }
    let is_sse = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    if !is_sse {
        return Err(LlmError::protocol("expected a text/event-stream response"));
    }
    Ok(response)
}

/// 讀錯誤 body,最多讀 `cap` bytes 就停。錯誤頁可能很大,不能整個讀進記憶體。
async fn read_capped(response: reqwest::Response, cap: usize) -> String {
    let mut buf = Vec::new();
    let mut body = response.bytes_stream();
    // 讀錯誤 body 本身失敗就算了:我們已經知道狀態碼,body 只是補充資訊。
    while let Some(Ok(chunk)) = body.next().await {
        let room = cap.saturating_sub(buf.len());
        buf.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
        if buf.len() >= cap {
            break;
        }
    }
    // `from_utf8_lossy`:不合法的 UTF-8 換成 �,這裡只是給人看的錯誤訊息,不必拒絕。
    String::from_utf8_lossy(&buf).into_owned()
}

/// reqwest 的錯誤 → 我們的錯誤。reqwest 的型別不外洩到公開 API。
fn transport(error: reqwest::Error) -> LlmError {
    if error.is_timeout() {
        return LlmError::Timeout;
    }
    // reqwest 的錯誤訊息很短,真正的原因在 `source()` 鏈裡,把整條鏈接起來。
    // Java 對照:像是把 `getCause()` 一路往下印。
    let mut message = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    LlmError::Transport(message)
}
