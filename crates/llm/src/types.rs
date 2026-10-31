//! 中立的對話型別:每個 provider adapter 都翻譯成這些,上層只認得這些。

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{LlmError, Result};
use crate::stop::StopReason;

/// 送給模型的一次請求。
// derive 是讓 compiler 自動產生 trait 實作,等於 Java 的 Lombok 或 IDE 自動產生的
// equals/toString。`Clone` = 可以深拷貝;`PartialEq` = 可以用 `==` 比較(測試要用)。
// 刻意不 derive `Debug`:自動產生的會把 system prompt 印進 log,改成下面手寫的版本。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub model: String,
    // `Option<String>` = 可能沒有值。Rust 沒有 null,「沒有」一定要寫進型別。
    // system 是獨立欄位而不是一種 Role:沿用 Go 專案的教訓,讓錯誤的寫法根本寫不出來。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    /// 各家專屬的設定(effort、thinking 顯示方式等),核心型別不認識它們。
    #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
    pub provider_options: ProviderMetadata,
}

impl Request {
    /// 送出前的基本檢查,跟 provider 無關。不合法就立刻報錯,連網路都不碰(寫入路徑 fail-loud)。
    pub fn validate(&self) -> Result<()> {
        if self.model.trim().is_empty() {
            return Err(LlmError::InvalidRequest("model must not be empty".into()));
        }
        if self.max_tokens == 0 {
            return Err(LlmError::InvalidRequest(
                "max_tokens must be at least 1".into(),
            ));
        }
        if self.messages.is_empty() {
            return Err(LlmError::InvalidRequest(
                "messages must not be empty".into(),
            ));
        }
        Ok(())
    }
}

/// 誰說的話。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
// 序列化時把 `User` 變成 "user"。
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// 對話裡的一輪。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    // 一輪可以混著好幾段內容,例如先說一句話、再呼叫工具。
    pub content: Vec<Content>,
}

/// 一段內容。
///
/// Rust 的 enum 每個 variant 都可以帶自己的資料,這是 Go 做不到的。Go 專案要用
/// 「unexported method 封閉 interface」才模擬出來,這裡一個 enum 就是封閉集合,
/// 而且 `match` 漏掉任何一種,compiler 都會報錯。
// 一樣不 derive `Debug`,理由同 `Request`。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
// JSON 長這樣:{"type": "text", "text": "..."}。`type` 欄位決定是哪一種 variant;
// 沒看過的 type 會解析失敗,而不是被默默丟掉(fail-closed)。
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    // 每一種內容都可以帶 `meta`:串流事件上的 provider 資料,組回 Content 時不能默默丟掉。
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
        meta: ProviderMetadata,
    },
    Thinking {
        text: String,
        /// thinking 簽章之類必須原樣傳回的資料。
        #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
        meta: ProviderMetadata,
    },
    ToolCall {
        id: String,
        name: String,
        /// 模型給的原始 JSON。不可信,交給工具層驗證後才解析。
        args_json: String,
        #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
        meta: ProviderMetadata,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

/// 提供給模型的工具定義。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema,原樣送給 provider。
    pub input_schema: Value,
}

/// token 用量。
// 全部是數字,`Copy` 讓它像 Go 的值型別一樣直接複製,不必 `.clone()`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

/// 收集完一個 stream 之後得到的完整回應。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub model: String,
    pub content: Vec<Content>,
    pub stop: StopReason,
    pub usage: Usage,
    /// `Finish` 事件帶來的 provider 資料。
    #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
    pub meta: ProviderMetadata,
}

/// 中立層「只保管、不解讀」的資料,以 provider 名稱為 key。
///
/// 例:`{"anthropic": {"signature": "..."}}`。
// 這種「只包一個欄位的 struct」叫 newtype。好處是型別名稱本身就說明用途,
// 外面也只能透過我們給的方法操作,不能把它當一般的 map 亂用。
// `transparent` = 序列化時直接當成裡面那個 map,不多包一層。
// 用 BTreeMap 而不是 HashMap:key 有固定順序,序列化結果穩定(prompt cache 也靠這個)。
// 一樣不 derive `Debug`:值裡可能有 user_id 之類的個資。
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderMetadata(BTreeMap<String, Value>);

impl ProviderMetadata {
    /// 取出某個 provider 的資料。
    // `&self` = 借用來讀。回傳 `Option<&Value>`:有就借給你看,沒有就是 None。
    pub fn get(&self, provider: &str) -> Option<&Value> {
        self.0.get(provider)
    }

    /// 放入某個 provider 的資料,同名的會被覆蓋。
    // `&mut self` = 借用來改。Rust 在型別上就區分「只讀」和「會改」,Go 和 Java 都沒有。
    // `impl Into<String>`:呼叫者傳 "anthropic"(&str)或 String 都可以。
    pub fn insert(&mut self, provider: impl Into<String>, value: Value) {
        self.0.insert(provider.into(), value);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 把 `other` 的資料併進來,同名的以 `other` 為準。
    // 參數是 `other: ProviderMetadata`(不是 `&`):呼叫者把整個 `other` 交給我們,之後不能再用它。
    // 所以可以直接把裡面的值「搬」過來,不必複製。Java 傳物件永遠是傳參考,沒有「交出所有權」這回事。
    pub fn merge(&mut self, other: ProviderMetadata) {
        self.0.extend(other.0);
    }
}

// ---- 手寫的 Debug:只印結構,不印內容(design.md §7 第 2 層) ----
//
// `impl 某 trait for 某型別` = 讓這個型別實作 trait。Java 的 `implements` 寫在 class 宣告上,
// Rust 可以在任何地方另外寫,所以「資料長什麼樣」和「它會做什麼」可以分開放。
// Go 對照:實作 `String()` 方法,但 Rust 會檢查簽章是否完全正確。

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("model", &self.model)
            // `.as_ref().map(...)`:有 system 就換成它的長度,沒有就維持 None。
            // `String::len()` 是 byte 數,不是字數:一個中文字佔 3 bytes。
            .field("system_bytes", &self.system.as_ref().map(String::len))
            .field("messages", &self.messages)
            .field("tools", &self.tools.len())
            .field("max_tokens", &self.max_tokens)
            .field("provider_options", &self.provider_options)
            .finish()
    }
}

impl fmt::Debug for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `match` 必須涵蓋每一種 variant,漏一種就編譯失敗。
        // 以後 enum 加新的 variant,這裡會被 compiler 逼著補上,不會忘記遮罩。
        match self {
            Content::Text { text, meta } => f
                .debug_struct("Text")
                .field("bytes", &text.len())
                .field("meta", meta)
                .finish(),
            Content::Thinking { text, meta } => f
                .debug_struct("Thinking")
                .field("bytes", &text.len())
                .field("meta", meta)
                .finish(),
            Content::ToolCall {
                id,
                name,
                args_json,
                meta,
            } => f
                .debug_struct("ToolCall")
                .field("id", id)
                .field("name", name)
                .field("args_bytes", &args_json.len())
                .field("meta", meta)
                .finish(),
            Content::ToolResult {
                call_id,
                content,
                is_error,
            } => f
                .debug_struct("ToolResult")
                .field("call_id", call_id)
                .field("bytes", &content.len())
                .field("is_error", is_error)
                .finish(),
        }
    }
}

impl fmt::Debug for ProviderMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 只印有哪些 provider 的 key,值一律不印。
        f.debug_set().entries(self.0.keys()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(s: &str) -> Content {
        Content::Text {
            text: s.to_owned(),
            meta: ProviderMetadata::default(),
        }
    }

    #[test]
    fn text_content_serializes_with_type_tag() {
        let v = serde_json::to_value(text("hi")).unwrap();
        assert_eq!(v, json!({ "type": "text", "text": "hi" }));
    }

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(Role::Assistant).unwrap(),
            json!("assistant")
        );
    }

    #[test]
    fn message_round_trips_every_content_kind() {
        let mut meta = ProviderMetadata::default();
        meta.insert("anthropic", json!({ "signature": "sig" }));
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                text("hi"),
                Content::Thinking {
                    text: "hmm".into(),
                    meta,
                },
                Content::ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    args_json: r#"{"path":"a.toml"}"#.into(),
                    meta: ProviderMetadata::default(),
                },
                Content::ToolResult {
                    call_id: "call_1".into(),
                    content: "ok".into(),
                    is_error: false,
                },
            ],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn unknown_content_type_is_rejected() {
        let r = serde_json::from_value::<Content>(json!({ "type": "image", "url": "x" }));
        assert!(r.is_err());
    }

    #[test]
    fn empty_provider_metadata_is_omitted() {
        let c = Content::Thinking {
            text: "t".into(),
            meta: ProviderMetadata::default(),
        };
        let v = serde_json::to_value(c).unwrap();
        assert_eq!(v, json!({ "type": "thinking", "text": "t" }));
    }

    #[test]
    fn provider_metadata_is_keyed_by_provider() {
        let mut meta = ProviderMetadata::default();
        meta.insert("anthropic", json!({ "signature": "sig" }));
        assert_eq!(meta.get("anthropic"), Some(&json!({ "signature": "sig" })));
        assert_eq!(meta.get("openai"), None);
    }

    #[test]
    fn debug_output_never_contains_conversation_text() {
        let req = Request {
            model: "claude-sonnet-5-5".into(),
            system: Some("SYSTEM-CANARY".into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![text("USER-CANARY")],
            }],
            tools: vec![],
            max_tokens: 1024,
            provider_options: ProviderMetadata::default(),
        };
        let out = format!("{req:?}");
        assert!(
            !out.contains("SYSTEM-CANARY"),
            "system prompt leaked: {out}"
        );
        assert!(!out.contains("USER-CANARY"), "user text leaked: {out}");
        assert!(
            out.contains("claude-sonnet-5-5"),
            "model should stay visible: {out}"
        );
    }

    #[test]
    fn content_debug_shows_the_length_instead_of_the_text() {
        // (cargo-mutants:`Content` 的 `fmt` 換成什麼都不印,原本沒有測試失敗。)
        let out = format!("{:?}", text("你好"));
        assert!(out.starts_with("Text { bytes: 6, meta: "), "{out}");
    }

    #[test]
    fn provider_metadata_debug_shows_keys_but_not_values() {
        let mut meta = ProviderMetadata::default();
        meta.insert("anthropic", json!({ "user_id": "VALUE-CANARY" }));
        let out = format!("{meta:?}");
        assert!(!out.contains("VALUE-CANARY"), "value leaked: {out}");
        assert!(
            out.contains("anthropic"),
            "provider key should stay visible: {out}"
        );
    }
}
