//! 資料集的一行:一個案例(design §4、D-029)。
//!
//! ```json
//! {"id": "ownership-001", "tags": ["rust"],
//!  "input": {"type": "chat", "max_tokens": 1024, "messages": [...]},
//!  "checks": [{"type": "contains", "value": "所有權"}],
//!  "rubric": ["說明 move 之後原本的變數不能再用"]}
//! ```

use std::fmt;

use llm::{Message, ProviderMetadata, Request, ToolDef};
use serde::{Deserialize, Serialize};

/// 一個案例:輸入、以及怎麼判斷回答好不好。
///
/// 不指定模型:模型是執行時的變數(`--model cheap`),同一個案例才能拿來比較不同模型。
// `deny_unknown_fields`:欄位打錯(`rubirc`)要報錯,不能默默變成「沒有評分標準」。
// 刻意不 derive `Debug`:私有資料集是真實對話,`reference` 可能是原本的回答,不能被印進 log。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// 在同一個資料集裡唯一;比較兩次執行時靠它配對。
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub input: Input,
    /// 確定性檢查(不需要呼叫 API)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    /// 給 LLM 評審的標準,一條一句,逐條判斷通過或不通過(D-031)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rubric: Vec<String>,
    /// 參考答案,只給評審看。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// 案例的輸入。現在只有一種;003 會加 `Agent`,004 會加 `Retrieval`(D-029)。
///
/// 加一個 variant 時,compiler 會列出每一個還沒處理它的 `match`。
/// Java 對照:sealed interface + pattern matching switch,新增 permitted 子類別時沒涵蓋的 switch 會編譯失敗。
// `tag = "type"`:JSON 裡用 `"type": "chat"` 決定是哪一種(internally tagged),跟 llm 的 `Content` 同一種寫法。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    Chat(ChatInput),
}

/// 一次 LLM 呼叫的輸入:`llm::Request` 少了 `model`。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
    pub provider_options: ProviderMetadata,
}

impl ChatInput {
    /// 補上執行時才決定的模型,變成真正要送出的請求。
    pub fn to_request(&self, model: &str) -> Request {
        Request {
            model: model.to_owned(),
            system: self.system.clone(),
            messages: self.messages.clone(),
            tools: self.tools.clone(),
            max_tokens: self.max_tokens,
            provider_options: self.provider_options.clone(),
        }
    }
}

/// 確定性檢查:程式自己就能判斷,不需要呼叫 API(D-031)。判斷的邏輯在第 2 步。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Check {
    /// 回答(去掉前後空白)完全等於 `value`。
    Exact {
        value: String,
    },
    Contains {
        value: String,
    },
    NotContains {
        value: String,
    },
    /// 回答符合正規表示式。
    Regex {
        pattern: String,
    },
    /// 回答是合法的 JSON,而且最上層有這些欄位。
    Json {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        required: Vec<String>,
    },
}

impl Case {
    /// 檢查一個案例本身合不合理,回傳全部的問題(空的 = 沒問題)。
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.id.is_empty() {
            problems.push("id must not be empty".into());
        } else if self.id.trim() != self.id {
            problems.push("id must not start or end with spaces".into());
        }
        if self.checks.is_empty() && self.rubric.is_empty() {
            problems.push(
                "needs at least one check or rubric item, otherwise it cannot be scored".into(),
            );
        }
        // 編號從 1 開始,跟人數的方式一樣。
        for (n, check) in (1..).zip(&self.checks) {
            if let Some(field) = check.empty_field() {
                problems.push(format!("check {n} has an empty {field}"));
            }
        }
        for (n, item) in (1..).zip(&self.rubric) {
            if item.trim().is_empty() {
                problems.push(format!("rubric item {n} is empty"));
            }
        }
        // 輸入的規則跟 llm 送出前的檢查共用同一份(`Request::validate`),不在這裡重寫一次。
        // 模型名稱要等執行時才知道,這裡放一個佔位的名字,只為了通過「模型不能是空的」那條。
        match &self.input {
            Input::Chat(chat) => {
                if let Err(e) = chat.to_request("model-chosen-at-run-time").validate() {
                    problems.push(invalid_request_message(e));
                }
            }
        }
        problems
    }
}

/// `LlmError::InvalidRequest("max_tokens must be at least 1")` → `max_tokens must be at least 1`。
fn invalid_request_message(error: llm::LlmError) -> String {
    match error {
        llm::LlmError::InvalidRequest(message) => message,
        other => other.to_string(),
    }
}

impl Check {
    /// 寫成空字串的欄位(`contains ""` 永遠通過、空的 pattern 什麼都符合:都是寫錯)。
    fn empty_field(&self) -> Option<&'static str> {
        match self {
            Check::Exact { value } | Check::Contains { value } | Check::NotContains { value }
                if value.is_empty() =>
            {
                Some("value")
            }
            Check::Regex { pattern } if pattern.is_empty() => Some("pattern"),
            _ => None,
        }
    }
}

impl fmt::Debug for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 只印結構:id、tag、各有幾條。輸入的內容由 llm 的型別自己遮罩,參考答案只印長度。
        f.debug_struct("Case")
            .field("id", &self.id)
            .field("tags", &self.tags)
            .field("input", &self.input)
            .field("checks", &self.checks.len())
            .field("rubric", &self.rubric.len())
            .field("reference_bytes", &self.reference.as_ref().map(String::len))
            .finish()
    }
}

impl fmt::Debug for ChatInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 跟 `llm::Request` 的 Debug 一樣:system prompt 只印長度,messages 交給 `Content` 自己遮罩。
        f.debug_struct("ChatInput")
            .field("system_bytes", &self.system.as_ref().map(String::len))
            .field("messages", &self.messages)
            .field("tools", &self.tools.len())
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}
