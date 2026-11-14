//! 打分流程:讀一次執行的回應,逐筆打分,寫進 `scores/<name>.jsonl`(design §6、D-031、D-034)。
//!
//! ```text
//! responses.jsonl ──每個 (案例, 抽樣) 取最後一筆──→ 要打分的回應
//!   ──扣掉這份打分已經評過的──→ 這次要評的
//!   ──確定性檢查──→ 沒過?(skip_on_check_failure)→ 不叫評審,直接不通過
//!                └→ 有評分標準 → 評審(並行上限、預算、逾時)
//!   ──→ 這筆回應的結果:pass / fail / incomplete / call_error ──→ scores/<name>.jsonl
//! ```
//!
//! 跟執行一樣「只往後加、可以接續」:同一個名字再打一次,只補還沒評的;設定不同就拒絕(不能混兩種評法)。

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use futures::StreamExt;
use jiff::Timestamp;
use llm::Usage;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::judge::error_verdicts;
use super::{Checks, Judge, Scored, Scorer, Verdict, VerdictResult};
use crate::case::{Case, Output};
use crate::dataset::{Dataset, DatasetError, sha256_hex};
use crate::run::{ExecOptions, Price, RunConfig, Spent, cost, plan, sum_usd};
use crate::store::{CallResult, ResponseLine, RunDir, RunMeta, ScoreFiles, StoreError};

/// 這次打分要怎麼評。
#[derive(Clone, Copy)]
pub struct ScoreSetup<'a> {
    /// 結果檔的名字:`scores/<name>.jsonl`。同一次執行可以有好幾份(換評審、改評分),互不覆蓋。
    pub name: &'a str,
    /// 沒有評審就只做確定性檢查;這時案例不能有評分標準(沒辦法評,就不開始)。
    pub judge: Option<&'a Judge>,
    /// 確定性檢查沒過就不叫評審(D-034)。`--judge-all` 把它關掉。
    pub skip_on_check_failure: bool,
}

/// 寫進 `<name>.meta.json` 的設定。同一個名字再打一次時必須完全一樣;`compare` 也靠它判斷兩份打分能不能比(design §7)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeConfig>,
    pub skip_on_check_failure: bool,
    /// 這次執行的案例的「評分方式」(檢查、評分標準、參考答案)的 SHA-256。
    /// 資料集可以在執行之後改評分方式(題目不能改),所以要另外記。
    pub criteria_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeConfig {
    pub model: String,
    pub effort: String,
}

/// `scores/<name>.meta.json`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreMeta {
    pub eval_version: String,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
    pub config: ScoreConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<ScoreSummary>,
}

/// `scores/<name>.jsonl` 的一行:一筆回應的打分。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreLine {
    pub case_id: String,
    pub sample: u32,
    /// 評的是 `responses.jsonl` 的第幾筆(從 0 開始)。那個 (案例, 抽樣) 後來有更新的回應時
    /// (`run --resume --retry-errors`),這筆打分就作廢,下次打分會重評。
    pub response: usize,
    pub result: ScoreResult,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verdicts: Vec<Verdict>,
    pub judge: JudgeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_usage: Option<Usage>,
    /// 評審的費用。沒叫評審、或沒有價格時是 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// 一筆回應的結果(design §6「案例的結果」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreResult {
    /// 每一條檢查與評分標準都通過。
    Pass,
    /// 至少一條沒通過。
    Fail,
    /// 沒有不通過的,但有打分器出錯的(`Error`):不知道算不算通過,單獨統計,報告要提醒「打分不完整」。
    Incomplete,
    /// 呼叫模型就失敗了,沒有回答可以評。算沒通過,錯誤率另外統計(design §5:跳過會讓分數虛高)。
    CallError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeStatus {
    /// 案例沒有評分標準,或沒有回答可以評。
    NotNeeded,
    Called,
    /// 確定性檢查已經沒過,評審改變不了結果,為了省錢沒叫(D-034)。這筆回應沒有評分標準的逐條結果。
    Skipped,
}

/// 整份打分的狀況(含之前接續前評過的)。寫進 `<name>.meta.json`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreSummary {
    /// 要打分的回應數:每個 (案例, 抽樣) 的最後一筆。
    pub responses: usize,
    pub pass: usize,
    pub fail: usize,
    pub incomplete: usize,
    pub call_errors: usize,
    /// 還沒評的(預算用完、中斷)。
    pub missing: usize,
    pub judge_calls: usize,
    pub judge_skipped: usize,
    /// 實際回答的評審模型 id。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub judge_models: Vec<String>,
    /// 評審的總用量與費用:每一次付了錢的評審呼叫都算(含後來作廢、重評的)。
    pub judge_usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// 打分的結果:摘要與要提醒使用者的事。
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreReport {
    pub summary: ScoreSummary,
    pub warnings: Vec<ScoreWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScoreWarning {
    /// 評審跟被評的是同一個模型:模型傾向偏好自己的回答(design §6)。
    #[error(
        "the judge and the graded model are the same ({}): models tend to prefer their own answers, so read these scores with care",
        models.join(", ")
    )]
    SelfPreference { models: Vec<String> },
}

/// 開始前的估價(`--dry-run`)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreEstimate {
    /// 最多要叫幾次評審(假設確定性檢查全部通過,沒有一次被省下來)。
    pub judge_calls: usize,
    pub worst_case_usd: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ScoreError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Dataset(#[from] DatasetError),

    #[error("--budget needs a price for the judge model; add it to [prices] in eval.toml")]
    BudgetWithoutPrice,

    #[error("case `{case_id}` has answers in this run but is no longer in the dataset")]
    CaseMissing { case_id: String },

    #[error(
        "the input of case `{case_id}` changed after it was answered, so the answers no longer match the question; start a new run"
    )]
    InputChanged { case_id: String },

    #[error("case `{case_id}` has a rubric, which needs a judge; pass --judge")]
    JudgeRequired { case_id: String },

    #[error(
        "score `{name}` already exists with different settings ({}); pick another --name",
        differences.join("; ")
    )]
    ConfigChanged {
        name: String,
        differences: Vec<String>,
    },
}

/// 估價:最多要叫幾次評審、最多花多少。不呼叫任何模型,也不寫任何檔案。
pub fn estimate(
    run_dir: &Path,
    setup: &ScoreSetup<'_>,
    price: Option<Price>,
) -> Result<ScoreEstimate, ScoreError> {
    let p = prepare(run_dir, setup)?;
    // 會叫評審的:有回答、案例有評分標準。檢查有沒有過要真的跑了才知道,這裡一律當成通過(最壞情況)。
    let calls: Vec<(&Case, &Output)> = match setup.judge {
        None => vec![],
        Some(_) => p
            .todo
            .iter()
            .filter_map(|&i| p.responses.get(i))
            .filter_map(|line| match &line.result {
                CallResult::Ok { output } => Some((p.case(&line.case_id)?, output)),
                CallResult::Error { .. } => None,
            })
            .filter(|(case, _)| !case.rubric.is_empty())
            .collect(),
    };
    // `zip` 兩個 Option:評審與價格都有,才算得出金額。
    let worst_case_usd = setup.judge.zip(price).map(|(judge, price)| {
        sum_usd(
            calls
                .iter()
                .map(|(case, output)| judge.worst_case_usd(case, output, price)),
        )
    });
    Ok(ScoreEstimate {
        judge_calls: calls.len(),
        worst_case_usd,
    })
}

/// 打分。同一個名字已經有打分時接續:只評還沒評的(或回應已經更新的)。
pub async fn score(
    run_dir: &Path,
    setup: &ScoreSetup<'_>,
    options: ExecOptions,
    now: Timestamp,
) -> Result<ScoreReport, ScoreError> {
    // 沒辦法遵守預算就不開始(fail-closed)。只做確定性檢查時不花錢,不需要價格。
    if setup.judge.is_some() && options.budget_usd.is_some() && options.price.is_none() {
        return Err(ScoreError::BudgetWithoutPrice);
    }
    let p = prepare(run_dir, setup)?;

    // 先寫 meta 再寫任何一行:中途當掉的話,接續時才有設定可以比對。開始時間保留第一次打分的。
    let mut meta = p.existing.clone().unwrap_or_else(|| ScoreMeta {
        eval_version: env!("CARGO_PKG_VERSION").into(),
        started_at: now,
        finished_at: None,
        config: p.config.clone(),
        summary: None,
    });
    p.files.write_meta(&meta)?;

    // 預算算的是整份打分:接續時,之前花的也算進去。
    let spent = Spent::new(sum_usd(p.previous.iter().filter_map(|l| l.cost_usd)));
    let mut out = p.files.appender()?;
    // 跟 `run` 一樣:`buffer_unordered` 在同一個任務裡輪流執行,直接借用 `p`、`setup`,不需要 `Arc`。
    let mut results = futures::stream::iter(p.todo.iter().copied())
        .map(|index| score_one(index, &p, setup, &options, &spent))
        .buffer_unordered(options.concurrency.max(1));
    // 只有這個迴圈寫檔:一次一行,不需要鎖。
    while let Some(line) = results.next().await {
        if let Some(line) = line {
            out.append(&line)?;
        }
    }
    drop(results);

    let all: Vec<ScoreLine> = p.files.read_lines()?;
    let summary = summarize(&p.responses, &p.latest, &all);
    let warnings = self_preference(&p, setup, &summary).into_iter().collect();
    meta.finished_at = Some(now);
    meta.summary = Some(summary.clone());
    p.files.write_meta(&meta)?;
    Ok(ScoreReport { summary, warnings })
}

/// 評一筆回應。需要評審但預算已經用完時回傳 `None`(留給下次接續)。
async fn score_one(
    index: usize,
    p: &Prepared,
    setup: &ScoreSetup<'_>,
    options: &ExecOptions,
    spent: &Spent,
) -> Option<ScoreLine> {
    let line = p.responses.get(index)?;
    let case = p.case(&line.case_id)?;
    let mut scored = ScoreLine {
        case_id: line.case_id.clone(),
        sample: line.sample,
        response: index,
        result: ScoreResult::CallError,
        verdicts: vec![],
        judge: JudgeStatus::NotNeeded,
        judge_model: None,
        judge_usage: None,
        cost_usd: None,
    };
    let output = match &line.result {
        CallResult::Ok { output } => output,
        // 沒有回答可以評:算沒通過(design §5),不叫評審。每一條標準都記成沒評。
        CallResult::Error { .. } => {
            let checks = case
                .checks
                .iter()
                .map(|check| skipped(Checks.name(), &check.to_string(), NO_ANSWER));
            // 有評分標準就一定有評審(`prepare` 擋過),所以這裡的名字一定拿得到。
            let judge = setup.judge.map_or("judge", |j| j.name());
            let rubric = case.rubric.iter().map(|c| skipped(judge, c, NO_ANSWER));
            scored.verdicts = checks.chain(rubric).collect();
            return Some(scored);
        }
    };

    scored.verdicts = Checks.score(case, output).await.verdicts;
    let judge = setup.judge.filter(|_| !case.rubric.is_empty());
    match judge {
        None => {}
        Some(judge) if setup.skip_on_check_failure && any_check_failed(&scored.verdicts) => {
            scored.judge = JudgeStatus::Skipped;
            let rubric = case.rubric.iter();
            scored
                .verdicts
                .extend(rubric.map(|c| skipped(judge.name(), c, CHECK_FAILED)));
        }
        Some(judge) => {
            // 在「開始」時檢查,已經在進行的照樣做完(同 `run`)。
            if let Some(budget) = options.budget_usd
                && spent.usd() >= budget
            {
                return None;
            }
            let judged = tokio::time::timeout(options.timeout, judge.score(case, output))
                .await
                .unwrap_or_else(|_| Scored {
                    verdicts: error_verdicts(
                        &case.rubric,
                        &format!("judge call timed out after {} s", options.timeout.as_secs()),
                    ),
                    ..Scored::default()
                });
            let cost = options.price.map(|price| cost(&judged.usage, price));
            if let Some(cost) = cost {
                spent.add(cost);
            }
            scored.judge = JudgeStatus::Called;
            scored.judge_model = judged.model;
            scored.judge_usage = Some(judged.usage);
            scored.cost_usd = cost;
            scored.verdicts.extend(judged.verdicts);
        }
    }
    scored.result = result_of(&scored.verdicts);
    Some(scored)
}

/// 評審跟被評的是不是同一個模型。比對別名與實際的 id 兩種名字:別名不同也可能指向同一個模型。
fn self_preference(
    p: &Prepared,
    setup: &ScoreSetup<'_>,
    summary: &ScoreSummary,
) -> Option<ScoreWarning> {
    let judge = setup.judge?;
    let graded: BTreeSet<&str> = p
        .latest
        .iter()
        .filter_map(|&i| p.responses.get(i))
        .filter_map(|line| match &line.result {
            CallResult::Ok { output } => Some(output.model()),
            CallResult::Error { .. } => None,
        })
        .chain([p.run_meta.config.model.as_str()])
        .collect();
    let judges: BTreeSet<&str> = summary
        .judge_models
        .iter()
        .map(String::as_str)
        .chain([judge.model()])
        .collect();
    // `BTreeSet::intersection`:兩邊都有的,依字母排序。Java 對照:`retainAll`。
    let models: Vec<String> = graded
        .intersection(&judges)
        .map(|m| (*m).to_owned())
        .collect();
    (!models.is_empty()).then_some(ScoreWarning::SelfPreference { models })
}

/// 開始前讀好、檢查好的一切。所有會讓打分「不該開始」的問題都在這裡擋下,還沒花任何錢(fail-loud)。
struct Prepared {
    files: ScoreFiles,
    run_meta: RunMeta,
    dataset: Dataset,
    responses: Vec<ResponseLine>,
    /// 每個 (案例, 抽樣) 最後一筆回應的索引,依檔案順序。
    latest: Vec<usize>,
    previous: Vec<ScoreLine>,
    config: ScoreConfig,
    existing: Option<ScoreMeta>,
    /// 這次要評的回應(索引)。
    todo: Vec<usize>,
}

impl Prepared {
    fn case(&self, id: &str) -> Option<&Case> {
        self.dataset.cases.iter().find(|case| case.id == id)
    }
}

fn prepare(run_dir: &Path, setup: &ScoreSetup<'_>) -> Result<Prepared, ScoreError> {
    let dir = RunDir::open(run_dir)?;
    let files = dir.scores(setup.name)?;
    let run_meta = dir.read_meta()?;
    let dataset = Dataset::load(&run_meta.dataset.path)?;
    let responses = dir.read_responses()?;
    let latest = latest(&responses);

    // id → 案例。Java 對照:`Map<String, Case>`;這裡存的是借用(`&Case`),不複製案例。
    let cases: HashMap<&str, &Case> = dataset.cases.iter().map(|c| (c.id.as_str(), c)).collect();
    for line in latest.iter().filter_map(|&i| responses.get(i)) {
        let Some(case) = cases.get(line.case_id.as_str()) else {
            return Err(ScoreError::CaseMissing {
                case_id: line.case_id.clone(),
            });
        };
        if case.input_sha256() != line.input_sha256 {
            return Err(ScoreError::InputChanged {
                case_id: line.case_id.clone(),
            });
        }
        if setup.judge.is_none() && !case.rubric.is_empty() {
            return Err(ScoreError::JudgeRequired {
                case_id: line.case_id.clone(),
            });
        }
    }

    let config = ScoreConfig {
        judge: setup.judge.map(|j| JudgeConfig {
            model: j.model().to_owned(),
            effort: j.effort().to_owned(),
        }),
        skip_on_check_failure: setup.skip_on_check_failure,
        criteria_sha256: criteria_sha256(&dataset, &run_meta.config),
    };
    let existing: Option<ScoreMeta> = files.read_meta()?;
    if let Some(old) = &existing
        && old.config != config
    {
        return Err(ScoreError::ConfigChanged {
            name: setup.name.to_owned(),
            differences: differences(&old.config, &config),
        });
    }

    let previous: Vec<ScoreLine> = files.read_lines()?;
    let scored = current_scores(&previous);
    let todo = latest
        .iter()
        .copied()
        .filter(|&i| {
            responses.get(i).is_some_and(|line| {
                let key = (line.case_id.as_str(), line.sample);
                // 評過、而且評的就是這一筆:不必再評。
                scored.get(&key).map(|s| s.response) != Some(i)
            })
        })
        .collect();

    Ok(Prepared {
        files,
        run_meta,
        dataset,
        responses,
        latest,
        previous,
        config,
        existing,
        todo,
    })
}

/// 每個 (案例, 抽樣) 最後一筆回應的索引,依檔案順序。
pub(crate) fn latest(responses: &[ResponseLine]) -> Vec<usize> {
    let last: HashMap<(&str, u32), usize> = responses
        .iter()
        .enumerate()
        .map(|(i, line)| ((line.case_id.as_str(), line.sample), i))
        // 同一個 key 後來的值覆蓋前面的:以最後一筆為準(同 `run::pending`)。
        .collect();
    let mut indexes: Vec<usize> = last.into_values().collect();
    // HashMap 的順序是隨機的,排序後才是檔案順序,結果才可重現。
    indexes.sort_unstable();
    indexes
}

/// (案例, 抽樣) → 這份打分裡最後一筆打分。
pub(crate) fn current_scores(lines: &[ScoreLine]) -> HashMap<(&str, u32), &ScoreLine> {
    lines
        .iter()
        .map(|line| ((line.case_id.as_str(), line.sample), line))
        .collect()
}

/// 這次執行的案例(依執行的篩選條件)的評分方式的 SHA-256。
fn criteria_sha256(dataset: &Dataset, config: &RunConfig) -> String {
    let mut seen = BTreeSet::new();
    let criteria: Vec<_> = plan(dataset, config)
        .iter()
        .filter(|item| seen.insert(item.case_index))
        .filter_map(|item| dataset.cases.get(item.case_index))
        .map(|case| json!([case.id, case.checks, case.rubric, case.reference]))
        .collect();
    sha256_hex(&serde_json::to_vec(&criteria).unwrap_or_default())
}

/// 兩份設定哪裡不同,寫成給人看的句子。
pub(crate) fn differences(old: &ScoreConfig, new: &ScoreConfig) -> Vec<String> {
    let judge = |j: &Option<JudgeConfig>| match j {
        None => "none".to_owned(),
        Some(j) => format!("{} (effort {})", j.model, j.effort),
    };
    let mut out = Vec::new();
    if old.judge != new.judge {
        out.push(format!(
            "judge: {} → {}",
            judge(&old.judge),
            judge(&new.judge)
        ));
    }
    if old.skip_on_check_failure != new.skip_on_check_failure {
        out.push(format!(
            "skip_on_check_failure: {} → {}",
            old.skip_on_check_failure, new.skip_on_check_failure
        ));
    }
    if old.criteria_sha256 != new.criteria_sha256 {
        out.push("the checks, rubric or reference answers changed".into());
    }
    out
}

const NO_ANSWER: &str = "no answer: the model call failed";
const CHECK_FAILED: &str = "not judged: a check already failed";

/// 一條沒評的標準。結果是 `Skipped` 而不是 `Fail`:不假造判決,但統計時算 0 分(design §7)。
fn skipped(scorer: &str, criterion: &str, reason: &str) -> Verdict {
    Verdict {
        scorer: scorer.to_owned(),
        criterion: criterion.to_owned(),
        result: VerdictResult::Skipped,
        reason: Some(reason.to_owned()),
    }
}

/// 確定性檢查有沒有「沒過」的。檢查自己出錯(`Error`)不算:評審的結果還能把它從不完整變成不通過。
fn any_check_failed(verdicts: &[Verdict]) -> bool {
    verdicts.iter().any(|v| v.result == VerdictResult::Fail)
}

/// 一筆回應的結果:有不通過就是不通過(評審出錯也改變不了);沒有不通過但有出錯或沒評的 = 不完整;
/// 全部通過才是通過。一條判決都沒有也是不完整:沒有證據就不能說它通過(fail-closed)。
fn result_of(verdicts: &[Verdict]) -> ScoreResult {
    let has = |result| verdicts.iter().any(|v| v.result == result);
    if has(VerdictResult::Fail) {
        ScoreResult::Fail
    } else if verdicts.is_empty() || has(VerdictResult::Error) || has(VerdictResult::Skipped) {
        ScoreResult::Incomplete
    } else {
        ScoreResult::Pass
    }
}

/// 從全部的打分算出摘要。結果的計數只看「目前有效」的打分(評的是每個 (案例, 抽樣) 最後一筆回應的);
/// 用量與費用則是每一次付了錢的評審呼叫都算。
fn summarize(responses: &[ResponseLine], latest: &[usize], lines: &[ScoreLine]) -> ScoreSummary {
    let current = current_scores(lines);
    let mut summary = ScoreSummary {
        responses: latest.len(),
        pass: 0,
        fail: 0,
        incomplete: 0,
        call_errors: 0,
        missing: 0,
        judge_calls: 0,
        judge_skipped: 0,
        judge_models: vec![],
        judge_usage: Usage::default(),
        cost_usd: None,
    };
    for &index in latest {
        let Some(response) = responses.get(index) else {
            continue;
        };
        let score = current
            .get(&(response.case_id.as_str(), response.sample))
            // 評的是舊的那筆回應:等於還沒評。
            .filter(|s| s.response == index);
        let Some(score) = score else {
            summary.missing += 1;
            continue;
        };
        // `match` 回傳一個 `&mut usize`,再對它加一:四個計數器共用同一行。
        *match score.result {
            ScoreResult::Pass => &mut summary.pass,
            ScoreResult::Fail => &mut summary.fail,
            ScoreResult::Incomplete => &mut summary.incomplete,
            ScoreResult::CallError => &mut summary.call_errors,
        } += 1;
        match score.judge {
            JudgeStatus::Called => summary.judge_calls += 1,
            JudgeStatus::Skipped => summary.judge_skipped += 1,
            JudgeStatus::NotNeeded => {}
        }
    }

    let called: Vec<&ScoreLine> = lines
        .iter()
        .filter(|l| l.judge == JudgeStatus::Called)
        .collect();
    for usage in called.iter().filter_map(|l| l.judge_usage) {
        let total = &mut summary.judge_usage;
        total.input_tokens += usage.input_tokens;
        total.output_tokens += usage.output_tokens;
        total.cache_read_tokens += usage.cache_read_tokens;
        total.cache_write_tokens += usage.cache_write_tokens;
    }
    let models: BTreeSet<&str> = called
        .iter()
        .filter_map(|l| l.judge_model.as_deref())
        .collect();
    summary.judge_models = models.into_iter().map(str::to_owned).collect();
    // 每一次評審都有費用才算得出總額;沒叫評審就是確定的 0(同 `run::summarize`:「未知」不能當成 0)。
    summary.cost_usd = called
        .iter()
        .all(|l| l.cost_usd.is_some())
        .then(|| sum_usd(called.iter().filter_map(|l| l.cost_usd)));
    summary
}

#[cfg(test)]
mod tests;
