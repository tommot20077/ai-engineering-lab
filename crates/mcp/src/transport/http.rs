//! Streamable HTTP 傳輸(design-part2 §5,規格 2026-07-28 basic/transports/streamable-http)。
//!
//! ```text
//! POST <url>   一個 JSON-RPC 請求一個 POST
//!   Accept: application/json, text/event-stream
//!   MCP-Protocol-Version / Mcp-Method / Mcp-Name / Mcp-Param-*   body 裡的值複製到 header,讓 gateway 不必解析 body
//!   Authorization: Bearer …                                        可選
//! ←  200 application/json    一份 JSON-RPC 回應
//! ←  200 text/event-stream   這個請求專屬的 SSE:先是幾個通知,最後是回應
//! ←  4xx                     本文常是 JSON-RPC 錯誤(400 + -32022 版本不支援、404 + -32601 方法不存在)
//! ```
//!
//! 2026-07-28 沒有 session:每個 POST 自己帶齊所有資訊,所以這個傳輸沒有狀態,關閉時也沒有東西要收。
//! 取消一個請求就是關掉它的回應串流(future 被 drop 時 reqwest 會關掉連線),規格不要求另外送通知。

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use super::Transport;
use crate::McpError;
use crate::headers::encode_value;
use crate::jsonrpc::{self, Id, Incoming};

/// 錯誤狀態碼的本文,放進錯誤訊息前最多讀多少、印多少。
const ERROR_BODY_BYTES: usize = 64 * 1024;
const ERROR_BODY_CHARS: usize = 512;

/// 怎麼連到 server。
///
/// `Debug` 是 derive 的:`SecretString` 自己的 `Debug` 只印 `[REDACTED]`,token 不會出現在 log 裡。
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// MCP endpoint。只收 https;`localhost`、`127.0.0.1`、`::1` 可以用 http。
    pub url: String,
    /// 可選的 bearer token(由組裝的地方從環境變數讀,library 不讀環境變數)。
    pub token: Option<SecretString>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpLimits {
    /// 一則訊息(JSON 本文、或 SSE 的一個事件)的上限。
    pub max_message_bytes: usize,
    /// 一個 SSE 回應串流全部加起來的上限(回應前面可以有很多通知)。
    pub max_stream_bytes: usize,
    /// 建立連線的上限。整個請求的上限由 `McpClient` 的 `request_timeout` 負責。
    pub connect_timeout: Duration,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: 4 * 1024 * 1024,
            max_stream_bytes: 16 * 1024 * 1024,
            connect_timeout: Duration::from_secs(10),
        }
    }
}

pub struct HttpTransport {
    client: reqwest::Client,
    url: url::Url,
    token: Option<SecretString>,
    limits: HttpLimits,
    next_id: AtomicI64,
}

impl HttpTransport {
    pub fn new(config: &HttpConfig, limits: HttpLimits) -> Result<Self, McpError> {
        let url = check_url(&config.url)?;
        let client = reqwest::Client::builder()
            // 不跟隨 redirect:跟過去的話,`Authorization` header 可能被帶到別的主機。3xx 直接當錯誤回報。
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(limits.connect_timeout)
            .build()
            .map_err(|e| McpError::Network(e.without_url().to_string()))?;
        Ok(Self {
            client,
            url,
            token: config.token.clone(),
            limits,
            next_id: AtomicI64::new(1),
        })
    }

    async fn post(
        &self,
        method: &str,
        params: Option<Value>,
        param_headers: Vec<(String, String)>,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let headers = metadata_headers(method, params.as_ref(), param_headers)?;
        let body = jsonrpc::request(id, method, params);
        let response = self.send(headers, body).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(error_from_status(status.as_u16(), response).await);
        }
        match content_type(&response).as_deref() {
            Some("application/json") => {
                let body = read_capped(response, self.limits.max_message_bytes).await?;
                match parse_response(&body, id)? {
                    Some(outcome) => outcome,
                    None => Err(McpError::Protocol(
                        "the response is not for this request".into(),
                    )),
                }
            }
            Some("text/event-stream") => self.read_stream(response, id).await,
            _ => Err(McpError::Protocol(format!(
                "expected application/json or text/event-stream, got HTTP {}{}",
                status.as_u16(),
                content_type(&response)
                    .map(|t| format!(" with {}", printable(&t)))
                    .unwrap_or_default()
            ))),
        }
    }

    async fn send(&self, headers: HeaderMap, body: String) -> Result<reqwest::Response, McpError> {
        let mut request = self
            .client
            .post(self.url.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .headers(headers)
            .body(body);
        if let Some(token) = &self.token {
            // `expose_secret`:只在真的要送出去的這一刻取出 token 的內容。
            request = request.bearer_auth(token.expose_secret());
        }
        // `without_url`:reqwest 的錯誤訊息預設含 URL,錯誤會被印出來、寫進 log,不帶比較保險。
        request
            .send()
            .await
            .map_err(|e| McpError::Network(e.without_url().to_string()))
    }

    /// SSE 回應:一個一個事件讀,略過通知,直到這個請求的回應出現。
    async fn read_stream(&self, response: reqwest::Response, id: i64) -> Result<Value, McpError> {
        let mut parser = llm::SseParser::with_max_event_bytes(self.limits.max_message_bytes);
        let mut stream = response.bytes_stream();
        let mut total = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| McpError::Network(e.without_url().to_string()))?;
            total = total.saturating_add(chunk.len());
            if total > self.limits.max_stream_bytes {
                return Err(McpError::TooLarge {
                    limit: self.limits.max_stream_bytes,
                });
            }
            // SSE 解析錯誤(一個事件超過上限之類)都是對方的問題。
            let events = parser
                .feed(&chunk)
                .map_err(|e| McpError::Protocol(e.to_string()))?;
            for event in events {
                if event.data.trim().is_empty() {
                    continue;
                }
                if let Some(outcome) = parse_response(event.data.as_bytes(), id)? {
                    // 回應到了就不再讀:`stream` 在這裡被 drop,連線關閉。
                    return outcome;
                }
            }
        }
        Err(McpError::Protocol(
            "the event stream ended without a response".into(),
        ))
    }
}

impl Transport for HttpTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(self.post(method, params, Vec::new()))
    }

    fn request_with_headers<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        headers: Vec<(String, String)>,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(self.post(method, params, headers))
    }

    /// 2026-07-28 的核心協定沒有 client 送給 server 的 HTTP 通知;還是照規格的機制送:server 接受就回 202。
    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            let headers = metadata_headers(method, params.as_ref(), Vec::new())?;
            let response = self
                .send(headers, jsonrpc::notification(method, params))
                .await?;
            match response.status().as_u16() {
                202 => Ok(()),
                status => Err(error_from_status(status, response).await),
            }
        })
    }

    /// 沒有 session,沒有東西要收:丟掉 client 就好。
    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// 只收 https;本機可以用 http(規則同 `config_loader::check_api_base_url`,library 不依賴 config-loader 所以再寫一次)。
/// URL 裡不能放帳號密碼:會被印進錯誤訊息,token 請用 `HttpConfig::token`。
fn check_url(raw: &str) -> Result<url::Url, McpError> {
    let url =
        url::Url::parse(raw).map_err(|e| McpError::InvalidUrl(format!("not a valid URL: {e}")))?;
    let local = match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err(McpError::InvalidUrl(
            "credentials in the URL are not allowed; use a token".into(),
        ));
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if local => Ok(url),
        _ => Err(McpError::InvalidUrl(
            "must use https (plain http is only allowed for localhost)".into(),
        )),
    }
}

/// 規格 Request Metadata:body 裡的值複製到 header。
fn metadata_headers(
    method: &str,
    params: Option<&Value>,
    param_headers: Vec<(String, String)>,
) -> Result<HeaderMap, McpError> {
    let mut headers = HeaderMap::new();
    let mut put = |name: &str, value: &str| -> Result<(), McpError> {
        // header 名稱與值都要合法;名稱來自工具定義(已經檢查過是 token),值已經編碼過。保險起見還是檢查。
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            McpError::Protocol(format!("invalid header name `{}`", printable(name)))
        })?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| McpError::Protocol("invalid header value".into()))?;
        headers.insert(name, value);
        Ok(())
    };
    put("Mcp-Method", &encode_value(method))?;
    let field = |key: &str| params.and_then(|p| p.get(key));
    if let Some(version) = field("_meta")
        .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str)
    {
        put("MCP-Protocol-Version", &encode_value(version))?;
    }
    // `Mcp-Name`:tools/call、prompts/get 用 `params.name`,resources/read 用 `params.uri`。
    let name = match method {
        "tools/call" | "prompts/get" => field("name"),
        "resources/read" => field("uri"),
        _ => None,
    };
    if let Some(name) = name.and_then(Value::as_str) {
        put("Mcp-Name", &encode_value(name))?;
    }
    for (name, value) in param_headers {
        put(&format!("Mcp-Param-{name}"), &value)?;
    }
    Ok(headers)
}

/// `Content-Type` 的主要部分(去掉 `; charset=utf-8` 之類的參數),小寫。
fn content_type(response: &reqwest::Response) -> Option<String> {
    let raw = response.headers().get(CONTENT_TYPE)?.to_str().ok()?;
    let essence = raw.split(';').next().unwrap_or("").trim();
    Some(essence.to_ascii_lowercase())
}

/// 讀完整個本文,超過上限就停(不會把整個超大的本文讀進記憶體)。
async fn read_capped(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, McpError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| McpError::Network(e.without_url().to_string()))?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(McpError::TooLarge { limit });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// 解析一則訊息。這個請求的回應 → `Some(結果)`;通知 → `None`(略過);其他都是違反協定。
fn parse_response(bytes: &[u8], id: i64) -> Result<Option<Result<Value, McpError>>, McpError> {
    let text = std::str::from_utf8(bytes).map_err(|_| McpError::Protocol("not UTF-8".into()))?;
    match jsonrpc::parse(text).map_err(|e| McpError::Protocol(e.to_string()))? {
        Incoming::Response {
            id: Id::Number(n),
            outcome,
        } if n == id => Ok(Some(outcome.map_err(McpError::Rpc))),
        Incoming::Response { .. } => {
            Err(McpError::Protocol("a response for another request".into()))
        }
        // 規格:回應串流上不能有 server 送的請求(2026-07-28 改用 MRTR)。
        Incoming::Request { .. } => Err(McpError::Protocol(
            "the server sent a request on a response stream".into(),
        )),
        Incoming::Notification { .. } => Ok(None),
    }
}

/// 錯誤狀態碼:本文是 JSON-RPC 的錯誤回應就交出 `Rpc`(-32022 由 client 再轉成 `UnsupportedVersion`),否則是 `Http`。
async fn error_from_status(status: u16, response: reqwest::Response) -> McpError {
    // 讀不到或太大的本文就當作空的:狀態碼本身已經說明了大部分。
    let body = read_capped(response, ERROR_BODY_BYTES)
        .await
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&body);
    if let Ok(Incoming::Response {
        outcome: Err(error),
        ..
    }) = jsonrpc::parse(&text)
    {
        return McpError::Rpc(error);
    }
    McpError::Http {
        status,
        body: printable(text.trim()),
    }
}

/// 要印出來的 server 字串:控制字元換成 `?`、最多 `ERROR_BODY_CHARS` 個字元。
fn printable(text: &str) -> String {
    text.chars()
        .take(ERROR_BODY_CHARS)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests;
