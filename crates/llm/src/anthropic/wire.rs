//! Anthropic Messages API 的 JSON 形狀(只照 API 長相定義,沒有任何邏輯)。
//!
//! 全部是 `pub(super)`:只有 `anthropic` 模組自己看得到。crate 的其他地方、更別說 crate 外面,
//! 都碰不到 Anthropic 的格式,這就是「vendor 型別不外洩」(design.md §2)在型別系統上的保證。
// Java 對照:`pub(super)` 有點像 package-private,但範圍可以精確指定到「上一層 module」。

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use crate::types::Role;

// ---- 送出:請求 body ----
//
// 這裡的 struct 都帶 `<'a>`:欄位是 `&'a str` 這種「借來的」字串,不是自己擁有的 `String`。
// 意思是「這個 WireRequest 借用了原本的 Request,活得不能比它久」。
// 好處:把整段對話轉成 JSON 時,不必先把每一段文字複製一份。對話越長,省得越多。
// Java 沒有這個概念:Java 的物件欄位永遠是參考,GC 會確保它不會先被回收;Rust 沒有 GC,
// 所以要用 lifetime 標註告訴 compiler「誰借了誰、借多久」,由 compiler 在編譯時檢查。

#[derive(Serialize)]
pub(super) struct WireRequest<'a> {
    pub(super) model: &'a str,
    pub(super) max_tokens: u32,
    pub(super) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) system: Option<&'a str>,
    pub(super) messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) tools: Vec<WireTool<'a>>,
    /// provider 專屬的額外欄位(`fallbacks`、`output_config`…)。
    // `flatten`:序列化時把這個 map 的 key 直接攤平到最外層,而不是包成 `"extra": {...}`。
    #[serde(flatten)]
    pub(super) extra: Map<String, Value>,
}

#[derive(Serialize)]
pub(super) struct WireMessage<'a> {
    pub(super) role: Role,
    pub(super) content: Vec<WireContent<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum WireContent<'a> {
    Text {
        text: &'a str,
    },
    Thinking {
        thinking: &'a str,
        signature: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        /// 已驗證過是合法 JSON 的原始文字,原樣放進 body,不重新解析、不重新編碼。
        input: &'a RawValue,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        #[serde(skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

// serde 的 `skip_serializing_if` 要一個 `fn(&T) -> bool`,所以多寫這個小函式。
fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Serialize)]
pub(super) struct WireTool<'a> {
    pub(super) name: &'a str,
    pub(super) description: &'a str,
    pub(super) input_schema: &'a Value,
}

// ---- 收回:串流事件 ----
//
// 收回的資料是自己擁有的 `String`(不是 `&str`):它來自網路,解析完原始 bytes 就會被丟掉。

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum WireStreamEvent {
    MessageStart {
        message: WireMessageStart,
    },
    ContentBlockStart {
        index: usize,
        content_block: WireBlockStart,
    },
    ContentBlockDelta {
        index: usize,
        delta: WireDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: WireMessageDelta,
        #[serde(default)]
        usage: WireUsage,
    },
    MessageStop,
    Ping,
    Error {
        error: WireError,
    },
    /// 沒看過的事件類型。Anthropic 可能加新的事件,依它的版本政策應該忽略,而不是當成錯誤。
    // `#[serde(other)]`:`type` 對不上任何一個 variant 時,就落到這裡。
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
pub(super) struct WireMessageStart {
    pub(super) id: Option<String>,
    pub(super) model: String,
    #[serde(default)]
    pub(super) usage: WireUsage,
}

/// 每個欄位都是 `Option`:`message_start` 和 `message_delta` 各自只帶一部分,欄位也可能是 `null`。
#[derive(Deserialize, Default)]
pub(super) struct WireUsage {
    pub(super) input_tokens: Option<u64>,
    pub(super) output_tokens: Option<u64>,
    pub(super) cache_read_input_tokens: Option<u64>,
    pub(super) cache_creation_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum WireBlockStart {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    /// 我們沒有開啟的功能(server tool、citations…)才會出現,所以當成協定錯誤。
    #[serde(other)]
    Unsupported,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum WireDelta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Deserialize)]
pub(super) struct WireMessageDelta {
    pub(super) stop_reason: Option<String>,
    #[serde(default)]
    pub(super) stop_details: Option<Value>,
}

#[derive(Deserialize)]
pub(super) struct WireError {
    // `type` 是 Rust 的保留字,不能當欄位名稱;用 `rename` 對應到 JSON 的 "type"。
    #[serde(rename = "type")]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) message: String,
}
