//! 一筆呼叫紀錄的形狀(design-part2.md §15、D-021)。序列化之後就是 JSONL 裡的一行。
//!
//! 讀它的人:階段 2 的 006 分析、007 訓練資料整理(Python)、002 Eval、除錯的你。
//! 欄位改了就要把 `SCHEMA_VERSION` 加一,讀檔的一方才知道這筆是哪一版。

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::LlmError;
use crate::types::{Request, Response, Usage};

/// 目前的紀錄格式版本。Java 對照:`serialVersionUID`。
pub const SCHEMA_VERSION: u32 = 1;

/// 一次呼叫的完整紀錄。
// 這裡 derive `Debug` 是安全的:`Request` 和 `Response` 的 Debug 都已經遮掉內容(第一部分 §7)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRecord {
    pub schema_version: u32,
    /// UUID v7:開頭是時間,依 id 排序就等於依時間排序。
    pub id: Uuid,
    /// 序列化成 RFC 3339 的 UTC 時間,例如 `2026-10-31T03:18:22.123Z`。
    pub started_at: Timestamp,
    /// 請求寫的名稱(通常是別名),也就是使用者的「意圖」。
    pub requested_model: String,
    /// 真正回答的模型(`Response.model`,含伺服器端 fallback)。從頭就失敗時是 `None`。
    pub served_model: Option<String>,
    /// 完整的請求,飛輪蒸餾時需要原封不動的輸入。
    pub request: Request,
    /// 完整的回應;`interrupted` / `cancelled` 時是收到的部分;`error` 時是 `None`。
    pub response: Option<Response>,
    pub outcome: Outcome,
    pub error: Option<RecordedError>,
    /// 從 `response` 複製出來,分析成本時不必往裡面挖。
    pub usage: Usage,
    pub latency_ms: Latency,
}

/// 這次呼叫的結果。
// `rename_all = "snake_case"`:序列化成 `"ok"`、`"cancelled"` 這種小寫字串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// 正常結束。飛輪主要用這種。
    Ok,
    /// 還沒有任何輸出就失敗了。
    Error,
    /// 輸出到一半斷線。
    Interrupted,
    /// 呼叫者中途不要了(例如使用者關掉視窗)。
    Cancelled,
}

/// 失敗的原因。只放分類、狀態碼和 `Display` 文字,不放上游的錯誤 body。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedError {
    /// 固定的分類字串,給程式判斷用:`http`、`transport`、`timeout`、`protocol`、`interrupted`、`invalid_request`。
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub message: String,
}

// `From<&LlmError>`:之後寫 `RecordedError::from(&e)` 或 `(&e).into()` 就能轉換。
// Java 對照:一個靜態工廠方法 `RecordedError.of(LlmError e)`,但 Rust 有統一的轉換介面。
impl From<&LlmError> for RecordedError {
    fn from(error: &LlmError) -> Self {
        // 沒有 `_ =>`:以後 `LlmError` 多一種 variant,compiler 會逼我們在這裡決定它的分類。
        let (kind, status) = match error {
            LlmError::Http { status, .. } => ("http", Some(*status)),
            LlmError::Transport(_) => ("transport", None),
            LlmError::Timeout => ("timeout", None),
            LlmError::Protocol(_) => ("protocol", None),
            LlmError::Interrupted { .. } => ("interrupted", None),
            LlmError::InvalidRequest(_) => ("invalid_request", None),
        };
        Self {
            kind: kind.to_owned(),
            status,
            // `Display` 只有狀態碼與簡短說明,不含上游 body(第一部分 §7)。
            message: error.to_string(),
        }
    }
}

/// 延遲,單位毫秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latency {
    /// 第一個有內容的事件花了多久。串流最重要的體驗指標。`complete()` 或沒有輸出時是 `None`。
    pub first_output: Option<u64>,
    /// 整段花了多久,包含 Retry 的等待(Record 在最外層,D-020)。
    pub total: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ProviderMetadata;

    #[test]
    fn record_serializes_to_the_documented_shape() {
        // 這是給 Python 讀的公開格式:欄位名稱一改,這個測試就要跟著改,提醒我們加 SCHEMA_VERSION。
        let record = CallRecord {
            schema_version: SCHEMA_VERSION,
            id: Uuid::nil(),
            started_at: "2026-10-31T03:18:22.418Z".parse().unwrap(),
            requested_model: "cheap".into(),
            served_model: None,
            request: Request {
                model: "cheap".into(),
                system: None,
                messages: vec![],
                tools: vec![],
                max_tokens: 1,
                provider_options: ProviderMetadata::default(),
            },
            response: None,
            outcome: Outcome::Error,
            error: Some((&LlmError::http(529, None, "secret body")).into()),
            usage: Usage::default(),
            latency_ms: Latency {
                first_output: None,
                total: 1250,
            },
        };

        let value = serde_json::to_value(&record).unwrap();

        assert_eq!(
            value,
            json!({
                "schema_version": 1,
                "id": "00000000-0000-0000-0000-000000000000",
                "started_at": "2026-10-31T03:18:22.418Z",
                "requested_model": "cheap",
                "served_model": null,
                // 空的 system、tools、provider_options 本來就不序列化(第一部分 types.rs)。
                "request": { "model": "cheap", "messages": [], "max_tokens": 1 },
                "response": null,
                "outcome": "error",
                "error": { "kind": "http", "status": 529, "message": "provider returned HTTP 529" },
                "usage": { "input_tokens": 0, "output_tokens": 0, "cache_read_tokens": 0, "cache_write_tokens": 0 },
                "latency_ms": { "first_output": null, "total": 1250 }
            })
        );
        // 上游 body 不能進紀錄。
        assert!(!value.to_string().contains("secret body"));
    }
}
