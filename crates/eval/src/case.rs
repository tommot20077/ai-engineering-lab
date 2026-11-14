//! 資料集的一行:一個案例(design §4、D-029)。
//!
//! ```json
//! {"id": "ownership-001", "tags": ["rust"],
//!  "input": {"type": "chat", "max_tokens": 1024, "messages": [...]},
//!  "checks": [{"type": "contains", "value": "所有權"}],
//!  "rubric": ["說明 move 之後原本的變數不能再用"]}
//! ```

use std::fmt;

use std::path::{Component, Path};

use jiff::Timestamp;
use llm::{Content, Message, ProviderMetadata, Request, Response, ToolDef, Usage};
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

/// 案例的輸入:一次 LLM 呼叫(`Chat`),或一次 agent 執行(`Agent`,003)。004 會加 `Retrieval`(D-029)。
///
/// 加一個 variant 時,compiler 會列出每一個還沒處理它的 `match`。
/// Java 對照:sealed interface + pattern matching switch,新增 permitted 子類別時沒涵蓋的 switch 會編譯失敗。
// `tag = "type"`:JSON 裡用 `"type": "chat"` 決定是哪一種(internally tagged),跟 llm 的 `Content` 同一種寫法。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    Chat(ChatInput),
    Agent(AgentInput),
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

/// 內建工具的名稱(`agent::tools`)。`Input::Agent` 的 `tools` 只能寫這些。
pub const BUILTIN_TOOLS: [&str; 4] = ["calculator", "current_time", "read_file", "list_dir"];

/// 一次 agent 執行的輸入(003 design §9):任務、可以用的內建工具。模型一樣在執行時才決定。
///
/// ```json
/// {"type": "agent", "prompt": "100 天後是幾月幾日?", "tools": ["current_time", "calculator"],
///  "max_tokens": 2048, "now": "2026-11-14T02:30:00Z"}
/// ```
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// 交給 agent 的任務(第一則 user 訊息)。
    pub prompt: String,
    /// 給 agent 的內建工具。空的 = 不給工具(對照組:同樣的題目、同樣的 loop,只差工具)。
    #[serde(default)]
    pub tools: Vec<String>,
    /// 每一輪模型回答的 token 上限。
    pub max_tokens: u32,
    /// 固定的時鐘:有「今天」的題目,答案才會每次一樣。沒寫就用真的時間。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now: Option<Timestamp>,
    /// 檔案工具的根目錄,相對於資料集檔案所在的目錄,必須在 `fixtures/` 底下。
    /// 不能是資料集目錄本身:那裡有評分標準,agent 讀得到就能作弊(Harness²)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// 最多幾輪;沒寫用 eval 的預設(`agent_run::EVAL_MAX_TURNS`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
}

impl AgentInput {
    /// 檔案工具需要根目錄。
    pub fn uses_files(&self) -> bool {
        self.tools
            .iter()
            .any(|t| t == "read_file" || t == "list_dir")
    }

    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.prompt.trim().is_empty() {
            problems.push("the agent prompt must not be empty".into());
        }
        if self.max_tokens == 0 {
            problems.push("max_tokens must be at least 1".into());
        }
        if self.max_turns == Some(0) {
            problems.push("max_turns must be at least 1".into());
        }
        for (i, tool) in self.tools.iter().enumerate() {
            if !BUILTIN_TOOLS.contains(&tool.as_str()) {
                problems.push(format!(
                    "unknown tool {tool:?}; built-in tools: {}",
                    BUILTIN_TOOLS.join(", ")
                ));
            } else if self.tools.iter().take(i).any(|earlier| earlier == tool) {
                problems.push(format!("tool {tool:?} is listed twice"));
            }
        }
        match (&self.root, self.uses_files()) {
            (None, true) => problems.push("read_file and list_dir need a root".into()),
            (Some(_), false) => problems.push("root is set but no file tool uses it".into()),
            (Some(root), true) if !is_fixture_dir(root) => problems.push(format!(
                "root {root:?} must be a relative path under fixtures/, like \"fixtures/<dataset>\""
            )),
            _ => {}
        }
        problems
    }
}

/// `fixtures/<名稱>[/...]`:只有一般的路徑段,第一段是 `fixtures`,至少兩段。
fn is_fixture_dir(root: &str) -> bool {
    let parts: Vec<Component> = Path::new(root).components().collect();
    parts.len() >= 2
        && parts.first() == Some(&Component::Normal("fixtures".as_ref()))
        && parts.iter().all(|c| matches!(c, Component::Normal(_)))
}

/// 一次執行的輸出,跟 `Input` 一一對應(D-029):`Input::Chat` 的輸出是 `Output::Chat`,`Input::Agent` 的是 `Output::Agent`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Output {
    Chat(Response),
    Agent(AgentOutput),
}

/// 一次 agent 執行的結果:怎麼結束的、每一步(含工具的參數與結果)、用量合計。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentOutput {
    /// 真正回答的模型(最後一步的 `Response.model`;一步都沒有時是請求的名稱)。
    pub model: String,
    pub outcome: agent::Outcome,
    pub steps: Vec<agent::Step>,
    pub usage: Usage,
}

impl Output {
    /// 實際回答的模型 id(別名換過之後的)。
    pub fn model(&self) -> &str {
        match self {
            Output::Chat(response) => &response.model,
            Output::Agent(output) => &output.model,
        }
    }

    /// 給確定性檢查用的文字:所有 text 區塊,用換行接起來。thinking 與工具呼叫不算(那不是給人看的回答)。
    pub fn text(&self) -> String {
        match self {
            Output::Chat(response) => response
                .content
                .iter()
                // `filter_map`:不是 text 的回傳 None 被濾掉,是的話取出裡面的字串。
                // Java 對照:`.filter(c -> c instanceof Text).map(c -> ((Text) c).text())`,一步做完。
                .filter_map(|block| match block {
                    Content::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            // 只有「回答了」才有文字;撞到護欄、失敗、拒答都是空字串,確定性檢查一律不通過(fail-closed)。
            Output::Agent(output) => match &output.outcome {
                agent::Outcome::Answered { text } => text.clone(),
                _ => String::new(),
            },
        }
    }
}

/// 確定性檢查:程式自己就能判斷,不需要呼叫 API(D-031)。判斷的邏輯在 `score/check.rs`。
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

/// 給人看的檢查描述,打分結果的 `criterion` 欄位就是它,例如 `contains "所有權"`。
// `{value:?}`:用 Debug 格式印字串,會加上引號並跳脫特殊字元;中文這類可以印出的字元照原樣顯示。
impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Check::Exact { value } => write!(f, "exact {value:?}"),
            Check::Contains { value } => write!(f, "contains {value:?}"),
            Check::NotContains { value } => write!(f, "not_contains {value:?}"),
            Check::Regex { pattern } => write!(f, "regex /{pattern}/"),
            Check::Json { required } if required.is_empty() => write!(f, "json"),
            Check::Json { required } => write!(f, "json with fields {}", required.join(", ")),
        }
    }
}

impl Case {
    /// 輸入的 SHA-256。每筆回應都記下它:打分時資料集可以改評分標準,但題目本身不能變(design §6)。
    pub fn input_sha256(&self) -> String {
        // serde_json 依欄位宣告的順序輸出,`ProviderMetadata` 是 `BTreeMap`(key 有排序):同樣的輸入一定得到同樣的 bytes。
        // 用 `HashMap` 的話順序每次執行都可能不同,hash 就不能存起來比較了。
        let bytes = serde_json::to_vec(&self.input).unwrap_or_default();
        crate::dataset::sha256_hex(&bytes)
    }

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
            } else if let Check::Regex { pattern } = check
                && let Err(e) = regex::Regex::new(pattern)
            {
                // 讀檔時就擋下,不要等花錢跑完模型、打分時才發現。
                problems.push(format!(
                    "check {n} has an invalid regex: {}",
                    regex_problem(&e)
                ));
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
            Input::Agent(agent) => problems.extend(agent.problems()),
        }
        problems
    }
}

/// regex 的錯誤訊息有好幾行(含一張指出位置的小圖),最後一行是原因:`error: unclosed group` → `unclosed group`。
pub(crate) fn regex_problem(error: &regex::Error) -> String {
    let message = error.to_string();
    let last = message.lines().last().unwrap_or_default();
    last.strip_prefix("error: ").unwrap_or(last).to_owned()
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

impl fmt::Debug for AgentInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 跟 `ChatInput` 一樣只印結構:任務與 system prompt 只印長度。
        f.debug_struct("AgentInput")
            .field("system_bytes", &self.system.as_ref().map(String::len))
            .field("prompt_bytes", &self.prompt.len())
            .field("tools", &self.tools)
            .field("max_tokens", &self.max_tokens)
            .field("now", &self.now)
            .field("root", &self.root)
            .field("max_turns", &self.max_turns)
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
