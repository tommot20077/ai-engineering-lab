//! 打分(design §6、D-031)。
//!
//! 每個打分器看一個案例與它的輸出,對每一條標準給出一個判決(`Verdict`):通過、不通過,或打分器本身出錯。

mod check;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::case::{Case, Output};

pub use check::Checks;

/// 打分器。現在有確定性檢查(`Checks`);第 4 步加 LLM 評審,之後也可能有其他的(例如 Jev)。
///
/// 評審要呼叫 LLM,所以是 async。回傳 `BoxFuture` 而不是寫成 `async fn`:
/// trait 裡的 `async fn` 回傳的型別每個實作都不同,沒辦法放進 `Vec<Box<dyn Scorer>>`(跟 `llm::LlmProvider` 同樣的理由)。
/// Java 對照:介面方法回傳 `CompletableFuture<List<Verdict>>`。
// `Send + Sync`:打分器會被好幾個 tokio 任務同時使用(第 5 步),要能安全地跨執行緒共享。
pub trait Scorer: Send + Sync {
    /// 寫進結果的 `scorer` 欄位,例如 `"check"`、`"judge"`。
    fn name(&self) -> &str;

    // `'a`:回傳的 future 會借用 `self`、`case`、`output`,所以三者都要活得比 future 久。
    fn score<'a>(&'a self, case: &'a Case, output: &'a Output) -> BoxFuture<'a, Vec<Verdict>>;
}

/// 對一條標準的判決。打分的結果檔(`scores/<name>.jsonl`)裡每一筆都是它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub scorer: String,
    /// 哪一條標準,給人看的:例如 `contains "所有權"`、評分標準的那一句。
    pub criterion: String,
    pub result: VerdictResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictResult {
    Pass,
    Fail,
    /// 打分器本身出錯(評審格式不對、regex 壞掉):不算通過,也不算不通過,單獨統計(design §6)。
    Error,
}
