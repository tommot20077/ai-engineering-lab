//! `eval calibrate` 的引擎(design-part2 §4):每個評審評校準集的每一筆回答,收集判決。
//!
//! 直接用正式打分的 `Judge`(prompt、分隔標記、structured outputs、嚴格驗證全部沿用),量到的就是打分時的評審。
//! 估價、`--budget`、並行上限、逾時的規則同 `run` / `score`;不做接續(一次約 $0.5,中斷就重跑)。

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use llm::Usage;
use serde::{Deserialize, Serialize};

use super::{CalibrationSet, Item, Label};
use crate::run::{Estimate, Price, Spent, cost, sum_usd};
use crate::score::{Judge, Scored, Scorer, Verdict, VerdictResult, error_verdicts};
use crate::{Case, Dataset};

/// 一個評審。
pub struct JudgeSetup<'a> {
    /// 報告上顯示的名字,也是送給 provider 的別名。
    pub alias: &'a str,
    /// 設定檔裡這個別名對應的模型 id:用來判斷「評審在評自己寫的回答」。
    pub model: &'a str,
    pub judge: &'a Judge,
    /// 沒有價格就算不出費用;`--budget` 需要每個評審都有價格。
    pub price: Option<Price>,
}

#[derive(Debug, Clone, Copy)]
pub struct CalibrateOptions {
    pub concurrency: usize,
    /// 每次評審呼叫的總時間上限。
    pub timeout: Duration,
    /// 全部評審累計花費超過就不再送出新的呼叫。
    pub budget_usd: Option<f64>,
}

/// 原始判決檔的一行:一個評審評一筆校準回答。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeLine {
    /// 評審的別名。
    pub judge: String,
    /// 校準回答的 id。
    pub item: String,
    /// 依標籤(= 評分標準)的順序,一條一個。
    pub verdicts: Vec<Verdict>,
    /// 實際回答的評審模型(別名換過之後的)。評審呼叫失敗時沒有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_model: Option<String>,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// 一次校準的結果。
#[derive(Debug, Clone, PartialEq)]
pub struct Calibration {
    /// 依評審(命令列的順序)、再依校準回答(檔案的順序)排好。
    pub lines: Vec<JudgeLine>,
    /// 計畫要做的呼叫數(評審數 × 回答數)。
    pub planned: usize,
}

impl Calibration {
    /// 預算用完、沒做的呼叫數。報告會標示「不完整」。
    pub fn skipped(&self) -> usize {
        self.planned.saturating_sub(self.lines.len())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CalibrateError {
    #[error("--budget needs a price for judge `{judge}`; add its model to [prices] in eval.toml")]
    NoPrice { judge: String },
}

/// 開始前的估價(`--dry-run`):每個評審評每一筆回答,最壞情況花多少。任何一個評審沒有價格時是 `None`。
pub fn estimate(set: &CalibrationSet, dataset: &Dataset, judges: &[JudgeSetup<'_>]) -> Estimate {
    let cases = cases(dataset);
    let per_judge = judges.iter().map(|setup| {
        // `?`:這個評審沒有價格,就是 `None`。
        let price = setup.price?;
        let usd = set.items.iter().filter_map(|item| {
            let case = cases.get(item.case_id.as_str())?;
            Some(setup.judge.worst_case_usd(case, &item.output(), price))
        });
        Some(sum_usd(usd))
    });
    Estimate {
        calls: judges.len() * set.items.len(),
        // `collect::<Option<Vec<_>>>()`:全部是 `Some` 才得到 `Some(Vec)`,只要有一個 `None` 整個就是 `None`。
        // 「任何一個評審沒有價格,就算不出最壞情況」一行寫完。Java 的 `Optional` 沒有這種收集方式。
        worst_case_usd: per_judge
            .collect::<Option<Vec<f64>>>()
            .map(|usd| sum_usd(usd.into_iter())),
    }
}

/// 案例 id → 案例。
fn cases(dataset: &Dataset) -> HashMap<&str, &Case> {
    dataset.cases.iter().map(|c| (c.id.as_str(), c)).collect()
}

/// 每個評審評每一筆回答。
pub async fn calibrate(
    set: &CalibrationSet,
    dataset: &Dataset,
    judges: &[JudgeSetup<'_>],
    options: CalibrateOptions,
) -> Result<Calibration, CalibrateError> {
    // 沒辦法遵守上限就不開始(fail-closed,同 run / score),而且要在送出任何呼叫之前。
    if options.budget_usd.is_some()
        && let Some(setup) = judges.iter().find(|j| j.price.is_none())
    {
        return Err(CalibrateError::NoPrice {
            judge: setup.alias.to_owned(),
        });
    }
    let cases = cases(dataset);
    // 每個工作是(第幾個評審, 第幾筆回答)。用索引而不是複製:回答可能很長。
    let work: Vec<(usize, usize)> = (0..judges.len())
        .flat_map(|j| (0..set.items.len()).map(move |i| (j, i)))
        .collect();
    let spent = Spent::new(0.0);
    // 跟 `run` / `score` 一樣:`buffer_unordered` 在同一個任務裡輪流執行,直接借用 `set`、`judges`,不需要 `Arc`。
    let mut done: Vec<((usize, usize), JudgeLine)> = futures::stream::iter(work.iter().copied())
        .map(|(j, i)| {
            let (cases, spent) = (&cases, &spent);
            async move {
                let setup = judges.get(j)?;
                let item = set.items.get(i)?;
                let line = judge_one(setup, item, cases, &options, spent).await?;
                Some(((j, i), line))
            }
        })
        .buffer_unordered(options.concurrency.max(1))
        .filter_map(|line| async move { line })
        .collect()
        .await;
    // 完成的順序不一定;排回「評審 → 回答」的順序,原始檔與報告才每次一樣。
    done.sort_by_key(|(key, _)| *key);
    Ok(Calibration {
        lines: done.into_iter().map(|(_, line)| line).collect(),
        planned: work.len(),
    })
}

/// 一個評審評一筆回答。預算用完時回傳 `None`(沒做)。
async fn judge_one(
    setup: &JudgeSetup<'_>,
    item: &Item,
    cases: &HashMap<&str, &Case>,
    options: &CalibrateOptions,
    spent: &Spent,
) -> Option<JudgeLine> {
    // 在「開始」時檢查,已經在進行的照樣做完(同 run / score)。
    if let Some(budget) = options.budget_usd
        && spent.usd() >= budget
    {
        return None;
    }
    let criteria: Vec<String> = item.labels.iter().map(|l| l.criterion.clone()).collect();
    let judged = match cases.get(item.case_id.as_str()) {
        // 讀校準集時已經對照過資料集;這裡還找不到,代表呼叫的人給了別的資料集。不猜,記成錯誤。
        None => Scored {
            verdicts: error_verdicts(&criteria, "the case is not in the dataset"),
            ..Scored::default()
        },
        Some(case) => {
            tokio::time::timeout(options.timeout, setup.judge.score(case, &item.output()))
                .await
                .unwrap_or_else(|_| Scored {
                    verdicts: error_verdicts(
                        &criteria,
                        &format!("judge call timed out after {} s", options.timeout.as_secs()),
                    ),
                    ..Scored::default()
                })
        }
    };
    // 評審真的回了(有模型 id)才算錢:逾時或沒送出時沒有用量,費用不是 $0,而是沒有。
    let cost_usd = judged
        .model
        .as_ref()
        .and(setup.price)
        .map(|price| cost(&judged.usage, price));
    if let Some(usd) = cost_usd {
        spent.add(usd);
    }
    Some(JudgeLine {
        judge: setup.alias.to_owned(),
        item: item.id.clone(),
        verdicts: align(&item.labels, judged.verdicts),
        judge_model: judged.model,
        usage: judged.usage,
        cost_usd,
    })
}

/// 依標籤的順序排好判決,一條標準一個。評審少給了某一條就記成 `Error`(不猜)。
fn align(labels: &[Label], verdicts: Vec<Verdict>) -> Vec<Verdict> {
    labels
        .iter()
        .map(|label| {
            verdicts
                .iter()
                .find(|v| v.criterion == label.criterion)
                .cloned()
                .unwrap_or_else(|| Verdict {
                    scorer: "judge".into(),
                    criterion: label.criterion.clone(),
                    result: VerdictResult::Error,
                    reason: Some("the judge gave no verdict for this criterion".into()),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests;
