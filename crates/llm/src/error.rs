//! 錯誤型別與可重試分類(design.md §5)。
//!
//! Rust 沒有 exception。會失敗的函式回傳 `Result<T, E>`:成功是 `Ok(T)`、失敗是 `Err(E)`,
//! 呼叫者「不處理就編譯不過」。這比 Go 的 `(T, error)` 更嚴格:Go 可以忘記檢查 err,
//! Rust 不行;也比 Java 的 checked exception 輕:它只是一個普通的回傳值。

use std::time::Duration;

use crate::types::Response;

/// 錯誤 body 最多保留多少 bytes。provider 回的錯誤頁可能很大,全部留著會吃記憶體、灌爆 log。
const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;

/// 這個 crate 的 `Result`:錯誤型別固定是 `LlmError`,寫 `Result<T>` 就好。
// `type` 是型別別名,等於 Go 的 `type X = Y`。std 的 io、fmt 也都這樣做。
pub type Result<T> = std::result::Result<T, LlmError>;

/// 呼叫 LLM 可能遇到的所有錯誤。
// `thiserror::Error` 會自動實作 `std::error::Error` 和 `Display`;
// 每個 variant 上的 `#[error("...")]` 就是它的 `Display` 文字。
// 錯誤訊息用英文、小寫開頭、結尾不加句點,這是 Rust 生態的慣例。
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// provider 回了非 2xx 的狀態碼。
    // `Display` 只印狀態碼,不印 body:body 可能夾帶使用者的輸入,錯誤訊息常常直接進 log。
    #[error("provider returned HTTP {status}")]
    Http {
        status: u16,
        /// provider 用 `retry-after` 告訴我們要等多久。
        retry_after: Option<Duration>,
        /// 已截斷到 `MAX_ERROR_BODY_BYTES` 以內。請用 `LlmError::http` 建立,才會自動截斷。
        body: String,
    },

    /// 連線、DNS、TLS 之類的網路錯誤。只存文字,不讓 reqwest 的型別外洩到公開 API。
    #[error("transport error: {0}")]
    Transport(String),

    #[error("request timed out")]
    Timeout,

    /// provider 送來壞掉的 SSE 或 JSON。重試通常也一樣壞,所以不可重試。
    #[error("malformed response from provider: {0}")]
    Protocol(String),

    /// 已經輸出過內容之後才斷線。`partial` 是斷線前收到的部分。
    // `Box` 把 `Response` 放到 heap 上。enum 的大小取決於最大的 variant,
    // 不裝箱的話,每個 `LlmError` 都會和 `Response` 一樣大,即使是小小的 `Timeout`。
    #[error("stream interrupted after output was produced")]
    Interrupted { partial: Box<Response> },

    /// 送出前的自我檢查沒過,例如 `max_tokens` 是 0。
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

impl LlmError {
    /// 建立 `Http` 錯誤,並把 body 截斷到上限以內。
    // `Self` 就是 `LlmError`,在 `impl` 區塊裡可以這樣簡寫。
    pub fn http(status: u16, retry_after: Option<Duration>, body: &str) -> Self {
        Self::Http {
            status,
            retry_after,
            body: truncate_on_char_boundary(body, MAX_ERROR_BODY_BYTES).to_owned(),
        }
    }

    /// 建立 `Protocol` 錯誤。`impl Into<String>`:傳 `&str` 或 `String`(例如 `format!` 的結果)都可以。
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    /// 這個錯誤值得重試嗎?之後的 retry middleware 只看這個答案。
    pub fn is_retryable(&self) -> bool {
        // 沒有 `_ =>` 萬用分支:以後加新的 variant,compiler 會逼我們在這裡決定它能不能重試。
        match self {
            // `matches!` 判斷值是否符合某個模式;`500..=599` 是包含兩端的範圍(529 overloaded 也在內)。
            Self::Http { status, .. } => matches!(status, 408 | 409 | 429 | 500..=599),
            Self::Transport(_) | Self::Timeout => true,
            // 已經給使用者看過的內容,不能偷偷重來一次(design.md §6)。
            Self::Protocol(_) | Self::Interrupted { .. } | Self::InvalidRequest(_) => false,
        }
    }
}

/// 讓 `?` 能把 JSON 解析錯誤自動轉成 `LlmError::Protocol`。
// `?` 的意思是:「`Err` 就立刻 return,而且先用 `From` 轉成這個函式的錯誤型別」。
// 等於 Go 的 `if err != nil { return nil, fmt.Errorf(...) }`,但只要一個字元。
impl From<serde_json::Error> for LlmError {
    fn from(e: serde_json::Error) -> Self {
        Self::Protocol(e.to_string())
    }
}

/// 截斷字串到 `max` bytes 以內,而且不切斷任何一個字。
// `String::len()` 算的是 bytes:英文 1 byte、中文 3 bytes、emoji 4 bytes。
// 直接在 byte 4096 切下去,可能切在一個中文字的中間,產生不合法的 UTF-8。
//
// 這裡回傳 `&str` 而且沒寫 lifetime:compiler 自動推斷「回傳值借用自 `s`」(lifetime elision)。
// 意思是:只要 `s` 還活著,回傳的切片就有效,而且不必複製任何資料。
fn truncate_on_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    // 往回找最近的字元邊界。位置 0 一定是邊界,所以這個迴圈一定會停。
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    // `s.get(..end)` 是不會 panic 的切片寫法(`&s[..end]` 會被 clippy 擋下)。
    s.get(..end).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_http_statuses_are_retryable() {
        for status in [408, 409, 429, 500, 502, 503, 529] {
            assert!(
                LlmError::http(status, None, "").is_retryable(),
                "{status} should be retryable"
            );
        }
    }

    #[test]
    fn client_http_statuses_are_not_retryable() {
        for status in [400, 401, 403, 404, 413] {
            assert!(
                !LlmError::http(status, None, "").is_retryable(),
                "{status} should not be retryable"
            );
        }
    }

    #[test]
    fn non_http_errors_classify_by_kind() {
        assert!(LlmError::Transport("connection reset".into()).is_retryable());
        assert!(LlmError::Timeout.is_retryable());
        assert!(!LlmError::Protocol("bad sse".into()).is_retryable());
        assert!(!LlmError::InvalidRequest("max_tokens is 0".into()).is_retryable());
    }

    #[test]
    fn http_body_is_capped_on_a_char_boundary() {
        // 「中」是 3 bytes,4096 不是 3 的倍數,所以截在 4096 會切斷一個字。
        let body = "中".repeat(2000);
        let err = LlmError::http(500, None, &body);
        let LlmError::Http { body: kept, .. } = err else {
            panic!("expected Http");
        };
        assert!(kept.len() <= MAX_ERROR_BODY_BYTES);
        assert!(body.starts_with(&kept));
        assert_eq!(kept.chars().count(), 1365);
    }

    #[test]
    fn display_shows_status_but_not_body() {
        let err = LlmError::http(429, None, "BODY-CANARY");
        let msg = err.to_string();
        assert!(msg.contains("429"), "{msg}");
        assert!(
            !msg.contains("BODY-CANARY"),
            "body leaked into Display: {msg}"
        );
    }

    #[test]
    fn question_mark_converts_json_errors_into_protocol_errors() {
        fn parse(raw: &str) -> Result<serde_json::Value> {
            Ok(serde_json::from_str(raw)?)
        }
        assert!(matches!(parse("{not json"), Err(LlmError::Protocol(_))));
    }
}
