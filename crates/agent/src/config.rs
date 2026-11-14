//! `agent.toml` 的形狀與檢查(design §8、§10-4)。
//!
//! 跟 eval、gateway 一樣分兩層:`RawConfig` 是檔案裡寫什麼就是什麼;`Settings` 是檢查過、補上預設值、
//! 路徑解析好的結果。讀檔與讀環境變數只在 `main.rs`(D-016),這裡全是純函式。

use std::collections::BTreeMap;
use std::path::PathBuf;

use config_loader::ConfigFile;
use llm::{DEFAULT_BASE_URL, RetryPolicy, Usage};
use serde::{Deserialize, Serialize};

use crate::limits::AgentLimits;
use crate::run::Step;

/// 沒寫 `[agent] model` 時用的別名。
pub const DEFAULT_MODEL_ALIAS: &str = "default";
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// 設定檔的原始內容。每個區段都 `deny_unknown_fields`:打錯的欄位名稱要報錯,不能被默默忽略。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    pub aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub anthropic: AnthropicSection,
    #[serde(default)]
    pub agent: AgentSection,
    /// 沒寫的欄位用預設值(`AgentLimits` 本身是 `#[serde(default)]`)。
    #[serde(default)]
    pub limits: AgentLimits,
    #[serde(default)]
    pub paths: PathsSection,
    #[serde(default)]
    pub prices: BTreeMap<String, Price>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AnthropicSection {
    pub base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentSection {
    /// `--model` 沒給時用的別名。
    pub model: String,
    pub max_tokens: u32,
}

impl Default for AgentSection {
    fn default() -> Self {
        AgentSection {
            model: DEFAULT_MODEL_ALIAS.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PathsSection {
    /// 軌跡放在 `<data_dir>/runs`。
    pub data_dir: Option<String>,
    /// 呼叫紀錄的目錄。預設跟 gateway 一樣:飛輪(階段 2)讀同一個地方。
    pub records: Option<String>,
}

/// 每百萬 token 的美元價格,key 是實際的模型 id(同 eval)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input: f64,
    pub output: f64,
}

/// 檢查過的設定。
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub aliases: BTreeMap<String, String>,
    pub base_url: String,
    /// 預設的模型別名。
    pub model: String,
    pub max_tokens: u32,
    pub limits: AgentLimits,
    pub prices: BTreeMap<String, Price>,
    /// 軌跡(`<data_dir>/runs`)。
    pub runs_dir: PathBuf,
    pub records_dir: PathBuf,
    pub retry: RetryPolicy,
}

/// 設定有問題。一次列出全部的問題。
#[derive(Debug, thiserror::Error)]
#[error("invalid agent config:{}", .0.iter().map(|p| format!("\n  - {p}")).collect::<String>())]
pub struct InvalidConfig(pub Vec<String>);

impl Settings {
    /// `default_data_dir`:某個程式的預設資料目錄。用函式傳進來,測試才不必碰真的家目錄。
    pub fn from_config(
        file: &ConfigFile<RawConfig>,
        default_data_dir: impl Fn(&str) -> config_loader::Result<PathBuf>,
    ) -> Result<Settings, InvalidConfig> {
        let raw = &file.value;
        let mut problems = Vec::new();

        if raw.aliases.is_empty() {
            problems.push("[aliases] must define at least one alias".into());
        }
        for (alias, model) in &raw.aliases {
            if alias.trim().is_empty() || model.trim().is_empty() {
                problems.push(format!(
                    "[aliases] `{alias}` must map a non-empty name to a non-empty model id"
                ));
            }
        }
        let base_url = config_loader::check_api_base_url(
            "anthropic.base_url",
            raw.anthropic
                .base_url
                .as_deref()
                .unwrap_or(DEFAULT_BASE_URL),
        )
        .map_err(|problem| problems.push(problem))
        .ok();
        if !raw.aliases.contains_key(&raw.agent.model) {
            problems.push(format!(
                "agent.model `{}` is not defined in [aliases]",
                raw.agent.model
            ));
        }
        if raw.agent.max_tokens == 0 {
            problems.push("agent.max_tokens must be at least 1".into());
        }
        for field in raw.limits.zero_fields() {
            problems.push(format!("limits.{field} must be at least 1"));
        }
        for (model, price) in &raw.prices {
            if let Some(id) = raw.aliases.get(model) {
                problems.push(format!(
                    "[prices] are keyed by model id, not alias: write [prices.\"{id}\"] instead of [prices.{model}]"
                ));
            }
            let valid = |v: f64| v.is_finite() && v >= 0.0;
            if !valid(price.input) || !valid(price.output) {
                problems.push(format!(
                    "[prices.{model}] input and output must be finite and not negative"
                ));
            }
        }
        let data_dir = match &raw.paths.data_dir {
            Some(dir) => file
                .resolve(dir)
                .map_err(|e| problems.push(format!("paths.data_dir: {e}")))
                .ok(),
            None => default_data_dir("agent")
                .map_err(|e| problems.push(format!("default data directory: {e}")))
                .ok(),
        };
        let records_dir = match &raw.paths.records {
            Some(dir) => file
                .resolve(dir)
                .map_err(|e| problems.push(format!("paths.records: {e}")))
                .ok(),
            None => default_data_dir("gateway")
                .map(|dir| dir.join("records"))
                .map_err(|e| problems.push(format!("default records directory: {e}")))
                .ok(),
        };

        // 只有「沒有任何問題」時,這些 Option 才一定都是 Some;寫在同一個 match 裡就不需要 unwrap。
        match (base_url, data_dir, records_dir) {
            (Some(base_url), Some(data_dir), Some(records_dir)) if problems.is_empty() => {
                Ok(Settings {
                    aliases: raw.aliases.clone(),
                    base_url,
                    model: raw.agent.model.clone(),
                    max_tokens: raw.agent.max_tokens,
                    limits: raw.limits.clone(),
                    prices: raw.prices.clone(),
                    runs_dir: data_dir.join("runs"),
                    records_dir,
                    retry: RetryPolicy::default(),
                })
            }
            _ => Err(InvalidConfig(problems)),
        }
    }
}

/// 整次執行的費用。每一步依「真正回答的模型」(`Response.model`)算;有任何一步的模型沒有價格就是 `None`
/// (顯示「未知」,不是 0)。cache 的讀寫一律按 input 價計:估高不估低(同 eval)。
pub fn cost(steps: &[Step], prices: &BTreeMap<String, Price>) -> Option<f64> {
    steps.iter().try_fold(0.0, |total, step| {
        let price = prices.get(&step.response.model)?;
        Some(total + step_cost(&step.response.usage, *price))
    })
}

fn step_cost(usage: &Usage, price: Price) -> f64 {
    let input = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
    (input as f64 * price.input + usage.output_tokens as f64 * price.output) / 1_000_000.0
}

#[cfg(test)]
mod tests;
