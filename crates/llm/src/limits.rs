//! 可調整的大小上限(D-016)。
//!
//! 預設值就放在各自使用它的 module 裡;這裡只是把它們集中成一個設定,由呼叫者透過 constructor 傳入。
//! library 自己不讀設定檔、不讀環境變數:那是 binary(之後的 gateway)的事。

/// 防止惡意或壞掉的回應把記憶體灌爆。
// `Copy`:只有兩個數字,傳來傳去直接複製,不必 `.clone()`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// 單一 SSE 事件最多多少 bytes。
    pub max_event_bytes: usize,
    /// 一則回覆的內容最多多少 bytes。
    pub max_response_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_event_bytes: crate::sse::MAX_EVENT_BYTES,
            max_response_bytes: crate::accumulate::MAX_RESPONSE_BYTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_limits() {
        // design.md §7:單一 SSE 事件 1 MiB、單次回應總量 32 MiB。寫成數字,常數算錯(例如 `*` 寫成 `+`)才抓得到。
        // (cargo-mutants 找到的缺口:原本沒有任何測試在看預設值本身。)
        assert_eq!(
            Limits::default(),
            Limits {
                max_event_bytes: 1_048_576,
                max_response_bytes: 33_554_432,
            }
        );
    }
}
