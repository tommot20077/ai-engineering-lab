//! 錯誤 → HTTP 回應(design-part2.md §17、D-023)。
//!
//! 原則:狀態碼要告訴呼叫者「該怎麼辦」。4xx = 你的請求有問題,改了再來;5xx = 不是你的錯。
//! 所以上游回 401(我們的 API key 錯了)不能原樣轉成 401,否則呼叫者會以為是自己的 gateway token 錯了。
//!
//! Java 對照:Spring 的 `@ControllerAdvice` 把內部例外轉成 HTTP 回應。
//! axum 的做法是讓錯誤型別實作 `IntoResponse`,handler 直接回傳 `Result<_, ApiError>`。

use std::time::Duration;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response as HttpResponse};
use llm::{LlmError, Response};
use serde::Serialize;

/// 回給呼叫者的錯誤。body 一律是 `{"error": {"kind", "message", "retryable", ...}}`。
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    /// 給程式判斷用的固定字串,例如 `rate_limited`、`upstream_error`。
    pub kind: &'static str,
    /// 給人看的說明。只用 `LlmError` 的 `Display`,不含上游 body(第一部分 §7)。
    pub message: String,
    /// 直接用 `LlmError::is_retryable()`:呼叫者不必自己背哪些狀態碼能重試。
    pub retryable: bool,
    /// 有值就送 `Retry-After` header。
    pub retry_after: Option<Duration>,
    /// 上游 400 的錯誤 body:通常在說呼叫者的請求哪裡不對,對呼叫者有用。其他情況一律不放。
    pub detail: Option<String>,
    /// 非串流請求輸出到一半斷線時,斷線前收到的部分(D-023)。
    pub partial: Option<Box<Response>>,
}

impl ApiError {
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retryable: false,
            retry_after: None,
            detail: None,
            partial: None,
        }
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "no such endpoint")
    }

    /// `{"error": {...}}` 的 JSON 文字。SSE 的 `error` 事件也用它,呼叫者只要寫一種解析方式。
    pub fn to_json(&self) -> String {
        // 只有字串、數字、布林與 `Response`(本來就能序列化),不會失敗;萬一失敗也要回一個合法的 JSON。
        serde_json::to_string(&self.envelope()).unwrap_or_else(|_| {
            r#"{"error":{"kind":"internal","message":"cannot encode error","retryable":false}}"#
                .into()
        })
    }

    fn envelope(&self) -> Envelope<'_> {
        Envelope {
            error: ErrorBody {
                kind: self.kind,
                message: &self.message,
                retryable: self.retryable,
                detail: self.detail.as_deref(),
                partial: self.partial.as_deref(),
            },
        }
    }
}

// 序列化用的「視圖」:借用 ApiError 的欄位,不複製。`<'a>` 說明它活得不比 ApiError 久。
#[derive(Serialize)]
struct Envelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    kind: &'a str,
    message: &'a str,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    partial: Option<&'a Response>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> HttpResponse {
        let mut response = (self.status, Json(self.envelope())).into_response();
        // RFC 7235:401 一定要附上 `WWW-Authenticate`,告訴對方該用哪種驗證方式。
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if let Some(wait) = self.retry_after
            && let Ok(value) = HeaderValue::from_str(&wait.as_secs().to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

/// `LlmError` → HTTP。對照表見 design-part2.md §17。
impl From<LlmError> for ApiError {
    fn from(error: LlmError) -> Self {
        // 先問完「能不能重試」,下面的 match 會把 error 拆開(move),之後就問不到了。
        let retryable = error.is_retryable();
        let message = error.to_string();
        let mut api = match error {
            LlmError::InvalidRequest(_) => {
                Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
            }
            // 上游說「你的請求不合法」:是呼叫者的問題,附上上游的說明(已截斷到 4 KiB)。
            LlmError::Http {
                status: 400, body, ..
            } => Self {
                detail: Some(body),
                ..Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
            },
            // 上游拒絕的是「gateway 的 API key」:是 gateway 的設定錯誤,不是呼叫者的錯。
            // 回 502,並在營運 log 大聲說出來(不印 body,裡面可能有 key 的片段)。
            LlmError::Http {
                status: status @ (401 | 403),
                ..
            } => {
                tracing::error!(
                    status,
                    "provider rejected the gateway's API key; check ANTHROPIC_API_KEY"
                );
                Self::new(StatusCode::BAD_GATEWAY, "upstream_auth", message)
            }
            // 呼叫者確實該放慢:原樣告訴它要等多久。
            LlmError::Http {
                status: 429,
                retry_after,
                ..
            } => Self {
                retry_after,
                ..Self::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", message)
            },
            LlmError::Http { .. } | LlmError::Transport(_) | LlmError::Protocol(_) => {
                Self::new(StatusCode::BAD_GATEWAY, "upstream_error", message)
            }
            LlmError::Timeout => {
                Self::new(StatusCode::GATEWAY_TIMEOUT, "upstream_timeout", message)
            }
            // 非串流請求輸出到一半斷線:把收到的部分交給呼叫者,讓它自己決定要不要用。
            LlmError::Interrupted { partial, cause } => Self {
                partial: Some(partial),
                ..Self::new(
                    StatusCode::BAD_GATEWAY,
                    "interrupted",
                    format!("{message}: {cause}"),
                )
            },
        };
        api.retryable = retryable;
        api
    }
}

/// body 不是合法的 JSON、content-type 不對、或格式不符合 `Request`。
/// axum 預設回純文字;這裡改成我們統一的 JSON 格式,狀態碼沿用 axum 判斷的(400 / 415 / 422 / 413)。
impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        let status = rejection.status();
        let kind = if status == StatusCode::PAYLOAD_TOO_LARGE {
            "payload_too_large"
        } else {
            "invalid_request"
        };
        Self::new(status, kind, rejection.body_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_errors_map_to_statuses_that_tell_the_caller_what_to_do() {
        // (錯誤, 狀態碼, kind, 可重試)。每一列對應 design-part2.md §17 的表格。
        let cases = [
            (
                LlmError::InvalidRequest("unknown model alias".into()),
                400,
                "invalid_request",
                false,
            ),
            (
                LlmError::http(400, None, "max_tokens too large"),
                400,
                "invalid_request",
                false,
            ),
            (LlmError::http(401, None, ""), 502, "upstream_auth", false),
            (LlmError::http(403, None, ""), 502, "upstream_auth", false),
            (
                LlmError::http(429, Some(Duration::from_secs(7)), ""),
                429,
                "rate_limited",
                true,
            ),
            (LlmError::http(529, None, ""), 502, "upstream_error", true),
            (LlmError::http(500, None, ""), 502, "upstream_error", true),
            (LlmError::http(404, None, ""), 502, "upstream_error", false),
            (
                LlmError::Transport("reset".into()),
                502,
                "upstream_error",
                true,
            ),
            (LlmError::protocol("bad sse"), 502, "upstream_error", false),
            (LlmError::Timeout, 504, "upstream_timeout", true),
        ];
        for (error, status, kind, retryable) in cases {
            let label = error.to_string();
            let api = ApiError::from(error);
            assert_eq!(
                (api.status.as_u16(), api.kind, api.retryable),
                (status, kind, retryable),
                "{label}"
            );
        }
    }

    #[test]
    fn upstream_body_is_shown_only_for_400() {
        // 400 的 body 在說呼叫者的請求哪裡錯,對呼叫者有用。
        let bad_request = ApiError::from(LlmError::http(400, None, "max_tokens too large"));
        assert_eq!(bad_request.detail.as_deref(), Some("max_tokens too large"));
        // 401 的 body 是關於「我們的 key」:不能給呼叫者看。
        let auth = ApiError::from(LlmError::http(401, None, "invalid x-api-key sk-ant-..."));
        assert_eq!(auth.detail, None);
        assert!(!auth.to_json().contains("x-api-key"));
    }

    #[test]
    fn rate_limit_carries_retry_after_as_a_header() {
        let api = ApiError::from(LlmError::http(429, Some(Duration::from_secs(7)), ""));
        let response = api.into_response();
        assert_eq!(response.headers()[header::RETRY_AFTER], "7");
    }

    #[test]
    fn interrupted_keeps_the_partial_answer_and_names_the_cause() {
        let partial = Response {
            id: None,
            model: "m".into(),
            content: vec![],
            stop: llm::StopReason {
                kind: llm::StopKind::Unknown,
                raw: "interrupted".into(),
            },
            usage: llm::Usage::default(),
            meta: llm::ProviderMetadata::default(),
        };
        let api = ApiError::from(LlmError::Interrupted {
            partial: Box::new(partial.clone()),
            cause: Box::new(LlmError::Transport("reset".into())),
        });
        assert_eq!((api.status.as_u16(), api.kind), (502, "interrupted"));
        assert_eq!(api.partial.as_deref(), Some(&partial));
        assert!(api.message.contains("reset"), "{}", api.message);
    }

    #[test]
    fn json_shape_is_the_documented_envelope() {
        let api = ApiError::from(LlmError::http(529, None, ""));
        let value: serde_json::Value = serde_json::from_str(&api.to_json()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "error": {
                "kind": "upstream_error",
                "message": "provider returned HTTP 529",
                "retryable": true
            }})
        );
    }
}
