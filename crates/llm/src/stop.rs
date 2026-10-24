//! 模型為什麼停下來。

use serde::{Deserialize, Serialize};

/// 停止原因:翻譯後的統一值 + provider 給的原值。
///
/// 兩個都留(參考 Vercel AI SDK):`kind` 給程式判斷,`raw` 給 log 和除錯。
/// 遇到沒看過的原因時 `kind` 是 `Unknown`,但 `raw` 不會遺失。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopReason {
    pub kind: StopKind,
    pub raw: String,
}

/// 統一的停止原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopKind {
    /// 正常說完。
    EndTurn,
    /// 撞到 `max_tokens`。看起來像完整回答,其實被截斷了。
    MaxTokens,
    /// 遇到指定的停止字串。
    StopSequence,
    /// 模型要呼叫工具。
    ToolUse,
    /// 模型拒答(安全分類)。
    Refusal,
    /// 伺服器端暫停(例如長時間的工具執行),要接著再送一次。
    PauseTurn,
    /// 沒看過的原因。上層應該當成失敗處理(fail-closed)。
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stop_kind_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(StopKind::EndTurn).unwrap(),
            json!("end_turn")
        );
    }

    #[test]
    fn unknown_stop_reason_keeps_raw_value() {
        let stop = StopReason {
            kind: StopKind::Unknown,
            raw: "brand_new_reason".into(),
        };
        let v = serde_json::to_value(&stop).unwrap();
        assert_eq!(v, json!({ "kind": "unknown", "raw": "brand_new_reason" }));
        let back: StopReason = serde_json::from_value(v).unwrap();
        assert_eq!(back, stop);
    }
}
