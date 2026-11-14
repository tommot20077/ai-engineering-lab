//! 護欄的數值(design §5、§10-3)。引擎自己強制,呼叫者和模型都繞不過;可以調整,不能關掉。

use serde::{Deserialize, Serialize};

/// agent 一次執行的上限。
// `deny_unknown_fields`:設定檔(`agent.toml` 的 `[limits]`)打錯字就拒絕,不默默忽略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentLimits {
    /// 最多呼叫模型幾次。
    pub max_turns: u32,
    /// 整次執行最多幾次工具呼叫。
    pub max_tool_calls: u32,
    /// 同一輪最多同時執行幾個工具。
    pub max_parallel: usize,
    /// 每輪 input + output(含 cache)的 token 加總上限。每輪都重送整段對話,input 會越滾越大。
    pub max_total_tokens: u64,
    /// 整次執行的總時間(秒)。
    pub deadline_secs: u64,
    /// 每次工具呼叫的時間上限(秒)。
    pub tool_timeout_secs: u64,
    /// 工具結果的大小上限(bytes),超過就截斷。
    pub max_result_bytes: usize,
    /// 工具參數(模型給的 JSON)的大小上限(bytes),超過就不解析、直接回錯誤。
    pub max_args_bytes: usize,
}

impl Default for AgentLimits {
    fn default() -> Self {
        AgentLimits {
            max_turns: 10,
            max_tool_calls: 30,
            max_parallel: 4,
            max_total_tokens: 200_000,
            deadline_secs: 300,
            tool_timeout_secs: 30,
            max_result_bytes: 32 * 1024,
            max_args_bytes: 64 * 1024,
        }
    }
}

impl AgentLimits {
    /// 每一項都至少是 1:0 等於把功能關掉(例如 `max_turns = 0` 永遠不會呼叫模型),那是設定錯誤。
    /// 回傳所有是 0 的欄位名稱,一次列出全部。
    pub fn zero_fields(&self) -> Vec<&'static str> {
        let fields = [
            ("max_turns", u64::from(self.max_turns)),
            ("max_tool_calls", u64::from(self.max_tool_calls)),
            ("max_parallel", self.max_parallel as u64),
            ("max_total_tokens", self.max_total_tokens),
            ("deadline_secs", self.deadline_secs),
            ("tool_timeout_secs", self.tool_timeout_secs),
            ("max_result_bytes", self.max_result_bytes as u64),
            ("max_args_bytes", self.max_args_bytes as u64),
        ];
        fields
            .into_iter()
            .filter(|(_, value)| *value == 0)
            .map(|(name, _)| name)
            .collect()
    }
}
