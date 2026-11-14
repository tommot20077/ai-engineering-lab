//! 執行一題 `Input::Agent`(003 design §9):組好工具與設定,跑一次 agent loop,結果變成 `Output::Agent`。
//!
//! provider 是 eval 執行器借給每一題的(`&dyn LlmProvider`,沒有 Record:D-033);agent 只借用它(`Agent<'p>`)。

use std::path::Path;
use std::sync::Arc;

use agent::tools::{Calculator, CurrentTime, ListDir, ReadFile, Root};
use agent::{Agent, AgentConfig, AgentFailure, AgentLimits, Clock, ErasedTool, Outcome, Toolbox};
use llm::{LlmProvider, ProviderMetadata};

use crate::case::{AgentInput, AgentOutput};

/// eval 裡每題預設最多幾輪(CLI 預設 10;eval 的題目都是幾步就能做完的)。
pub const EVAL_MAX_TURNS: u32 = 8;
/// eval 裡每題的 token 合計上限。也是估價的依據(`worst_case`)。
pub const EVAL_MAX_TOTAL_TOKENS: u64 = 60_000;

/// 一題 agent 跑不起來或模型呼叫失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRunError {
    /// 設定或工具組不合法(根目錄不存在之類):是資料集或環境的問題。
    Setup(String),
    /// agent 裡的某次模型呼叫失敗(Retry 之後)。跟 `Input::Chat` 的呼叫失敗一樣記成錯誤,不算模型答錯:
    /// 錯誤率分開統計,`--retry-errors` 可以重做(design §5)。
    Model(String),
}

/// 這一題的護欄。
pub fn limits(input: &AgentInput) -> AgentLimits {
    AgentLimits {
        max_turns: input.max_turns.unwrap_or(EVAL_MAX_TURNS),
        max_total_tokens: EVAL_MAX_TOTAL_TOKENS,
        ..AgentLimits::default()
    }
}

/// 根目錄:相對於資料集檔案所在的目錄。
pub fn root_of(input: &AgentInput, dataset_dir: &Path) -> Option<std::path::PathBuf> {
    input.root.as_ref().map(|root| dataset_dir.join(root))
}

fn toolbox(input: &AgentInput, dataset_dir: &Path) -> Result<Toolbox, AgentRunError> {
    let root = match root_of(input, dataset_dir) {
        Some(path) => Some(Root::new(&path).map_err(|e| {
            AgentRunError::Setup(format!("root {} is not usable: {e}", path.display()))
        })?),
        None => None,
    };
    let mut tools: Vec<Box<dyn ErasedTool>> = Vec::new();
    // 照案例裡寫的順序放:送給模型的工具順序固定,prompt cache 才吃得到。
    for name in &input.tools {
        let tool: Box<dyn ErasedTool> = match (name.as_str(), &root) {
            ("calculator", _) => Box::new(Calculator),
            ("current_time", _) => Box::new(CurrentTime),
            ("read_file", Some(root)) => Box::new(ReadFile::new(root.clone())),
            ("list_dir", Some(root)) => Box::new(ListDir::new(root.clone())),
            // 讀資料集時已經檢查過名稱與根目錄(`AgentInput::problems`);到這裡就是程式寫錯。
            _ => return Err(AgentRunError::Setup(format!("cannot build tool {name:?}"))),
        };
        tools.push(tool);
    }
    Toolbox::new(tools).map_err(|e| AgentRunError::Setup(e.to_string()))
}

/// 跑一題。
pub async fn run(
    input: &AgentInput,
    model: &str,
    provider: &dyn LlmProvider,
    dataset_dir: &Path,
) -> Result<AgentOutput, AgentRunError> {
    let toolbox = toolbox(input, dataset_dir)?;
    let config = AgentConfig {
        model: model.to_owned(),
        system: input.system.clone(),
        max_tokens: input.max_tokens,
        limits: limits(input),
        clock: input.now.map_or(Clock::System, Clock::Fixed),
        provider_options: ProviderMetadata::default(),
    };
    let agent = Agent::new(provider, Arc::new(toolbox), config)
        .map_err(|e| AgentRunError::Setup(e.to_string()))?;
    let finished = agent.run(input.prompt.clone()).await;
    if let Outcome::Failed {
        failure: AgentFailure::Model { message },
    } = &finished.outcome
    {
        return Err(AgentRunError::Model(message.clone()));
    }
    let served = finished
        .steps
        .last()
        .map_or_else(|| model.to_owned(), |s| s.response.model.clone());
    Ok(AgentOutput {
        model: served,
        outcome: finished.outcome,
        steps: finished.steps,
        usage: finished.usage,
    })
}

/// 一題最多花多少:token 合計的上限(最多超出一輪,用題目本身的大小與工具說明的寬限估),
/// 加上每一輪的輸出都用完 `max_tokens`。token 合計裡已經含輸出,所以這是高估(估高不估低,同 `run::worst_case`)。
pub fn worst_case_tokens(input: &AgentInput) -> (f64, f64) {
    /// 工具說明(JSON Schema)的寬限:四個內建工具加起來約 2 KB。
    const TOOL_DEFS_BYTES: usize = 4096;
    let limits = limits(input);
    let prompt_bytes =
        input.prompt.len() + input.system.as_ref().map_or(0, String::len) + TOOL_DEFS_BYTES;
    let input_tokens = limits.max_total_tokens as f64 + prompt_bytes as f64 / 2.0;
    let output_tokens = f64::from(limits.max_turns) * f64::from(input.max_tokens);
    (input_tokens, output_tokens)
}

#[cfg(test)]
mod tests;
