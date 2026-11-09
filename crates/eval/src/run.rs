//! 執行:對資料集真的呼叫模型,把每次的結果寫進 `responses.jsonl`(design §5、D-032 ~ D-035)。
//!
//! ```text
//! 資料集 ──篩選(tags / limit)──→ 工作清單:(案例, 第幾次抽樣) × N
//!          ──扣掉已經做過的(--resume)──→ 這次要做的
//!          ──並行上限、預算上限、逾時──→ provider.complete() ──→ responses.jsonl
//! ```
//!
//! 只負責「呼叫並記下來」,不打分(打分是第 5 步):改了評分方式,不必重付呼叫模型的錢。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use jiff::Timestamp;
use llm::{LlmError, LlmProvider, RecordedError, Request, Usage};
use serde::{Deserialize, Serialize};

use crate::case::{Case, Input, Output};
use crate::dataset::{Dataset, DatasetError};
use crate::store::{CallResult, DatasetInfo, ResponseLine, RunDir, RunMeta, StoreError};

/// 這次執行的設定。全部寫進 `meta.json`;接續時從那裡讀回來,保證同一次執行只有一種設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunConfig {
    /// 送給 provider 的模型名稱(通常是別名,例如 `cheap`;實際的 model id 記在每一筆回應裡)。
    pub model: String,
    /// 每題問幾次。
    pub repeats: u32,
    /// 只跑帶有其中任一個 tag 的案例;空的 = 全部。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// 篩選之後只跑前幾個案例。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// 給人看的說明,例如「新 system prompt」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// 這次呼叫要用的限制。每次呼叫 `start` / `resume` 都可以不同(例如接續時把預算調高),所以不寫進 meta。
/// 打分(`score::score`)也用它:那邊付錢的是評審,所以 `price` 是評審模型的價格。
#[derive(Debug, Clone, Copy)]
pub struct ExecOptions {
    /// 同時進行的呼叫上限。
    pub concurrency: usize,
    /// 每次呼叫的總時間上限。
    pub timeout: Duration,
    /// 這次執行(含之前做過的)累計花費超過就不再送出新的呼叫(D-034)。
    pub budget_usd: Option<f64>,
    /// 要付錢的那個模型的價格(執行:被測模型;打分:評審)。沒有就算不出費用。
    pub price: Option<Price>,
}

/// 每百萬 token 的美元價格。由使用者填在 `eval.toml`,不寫死在程式裡(價格會變)。
// `deny_unknown_fields`:設定檔裡打成 `ouput` 要報錯,不能變成「沒有輸出價格」。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input: f64,
    pub output: f64,
}

/// 一個工作:第幾個案例的第幾次抽樣。用索引而不是複製案例:案例可能很大。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkItem {
    pub case_index: usize,
    pub sample: u32,
}

/// 開始前的估價(`--dry-run` 印的就是它)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    pub calls: usize,
    /// 最壞情況:輸入用 bytes ÷ 2 粗估 token(刻意高估),輸出用 `max_tokens`。沒有價格時是 `None`。
    pub worst_case_usd: Option<f64>,
}

/// 整次執行的狀況(含之前接續前做過的)。寫進 `meta.json`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// 篩選後的 (案例, 抽樣) 總數。
    pub planned: usize,
    pub ok: usize,
    pub errors: usize,
    /// 還沒有結果的(預算用完、中斷)。大於 0 時報告會標出「這次執行不完整」。
    pub missing: usize,
    /// 所有呼叫(含重試過的失敗)的總花費。沒有價格時是 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Dataset(#[from] DatasetError),

    #[error("--budget needs a price for the model; add it to [prices] in eval.toml")]
    BudgetWithoutPrice,

    #[error(
        "the dataset {} changed since this run started (sha256 {expected} → {found}); start a new run instead",
        path.display()
    )]
    DatasetChanged {
        path: PathBuf,
        expected: String,
        found: String,
    },
}

/// 依設定列出全部的工作:篩選案例,每個案例展開成 `repeats` 次抽樣。順序跟資料集一樣。
pub fn plan(dataset: &Dataset, config: &RunConfig) -> Vec<WorkItem> {
    let selected = dataset
        .cases
        .iter()
        .enumerate()
        .filter(|(_, case)| {
            config.tags.is_empty() || case.tags.iter().any(|tag| config.tags.contains(tag))
        })
        .map(|(index, _)| index)
        // `take(usize::MAX)` 等於不限制:用同一條迭代器鏈處理「有 / 沒有 limit」,不必寫兩個分支。
        .take(config.limit.unwrap_or(usize::MAX));
    // `flat_map`:每個案例展開成好幾個工作。Java 對照:`stream().flatMap(...)`。
    selected
        .flat_map(|case_index| {
            (0..config.repeats).map(move |sample| WorkItem { case_index, sample })
        })
        .collect()
}

/// 扣掉已經有結果的。同一個 (案例, 抽樣) 有好幾行時,以最後一行為準;
/// 失敗的預設也算做過(結果才可重現),`retry_errors` 才重做。
pub fn pending(
    dataset: &Dataset,
    planned: &[WorkItem],
    done: &[ResponseLine],
    retry_errors: bool,
) -> Vec<WorkItem> {
    let last = last_results(done);
    planned
        .iter()
        .filter(|item| {
            let key = (dataset_id(dataset, item), item.sample);
            match last.get(&key) {
                None => true,
                Some(ok) => retry_errors && !ok,
            }
        })
        .copied()
        .collect()
}

/// (案例 id, 抽樣) → 最後一行是不是成功。後面的行蓋掉前面的。
fn last_results(lines: &[ResponseLine]) -> HashMap<(&str, u32), bool> {
    lines
        .iter()
        .map(|line| {
            let ok = matches!(line.result, CallResult::Ok { .. });
            ((line.case_id.as_str(), line.sample), ok)
        })
        // `collect` 成 HashMap 時,同一個 key 後來的值會覆蓋前面的:剛好就是「以最後一行為準」。
        .collect()
}

fn dataset_id<'a>(dataset: &'a Dataset, item: &WorkItem) -> &'a str {
    dataset
        .cases
        .get(item.case_index)
        .map_or("", |case| case.id.as_str())
}

/// 最壞情況的估價。
pub fn estimate(dataset: &Dataset, items: &[WorkItem], price: Option<Price>) -> Estimate {
    let worst_case_usd = price.map(|price| {
        items
            .iter()
            .filter_map(|item| dataset.cases.get(item.case_index))
            .map(|case| worst_case(case, price))
            .sum()
    });
    Estimate {
        calls: items.len(),
        worst_case_usd,
    }
}

/// 一次呼叫最多花多少:輸入用 bytes ÷ 2 當 token 數(中文一個字 3 bytes、約 1~2 token;英文約 4 字元一個 token,
/// 所以這是高估),輸出假設 `max_tokens` 全部用完(thinking 也算在 `max_tokens` 裡)。
fn worst_case(case: &Case, price: Price) -> f64 {
    match &case.input {
        Input::Chat(chat) => worst_case_request(&chat.to_request("model"), price),
    }
}

/// 一個請求最多花多少(規則見 `worst_case`)。評審的估價也用它(`score::estimate`)。
pub(crate) fn worst_case_request(request: &Request, price: Price) -> f64 {
    let bytes = serde_json::to_vec(request).map_or(0, |body| body.len());
    let input = bytes as f64 / 2.0;
    let output = f64::from(request.max_tokens);
    (input * price.input + output * price.output) / 1_000_000.0
}

/// 從全部的結果算出摘要。同一個 (案例, 抽樣) 以最後一行為準。
/// 費用:每一筆成功的呼叫都有費用才算得出總額,否則是 `None`(「未知」不能當成 0);失敗的呼叫沒有用量,算 0。
pub fn summarize(dataset: &Dataset, planned: &[WorkItem], lines: &[ResponseLine]) -> Summary {
    let last = last_results(lines);
    let (mut ok, mut errors) = (0, 0);
    for item in planned {
        match last.get(&(dataset_id(dataset, item), item.sample)) {
            Some(true) => ok += 1,
            Some(false) => errors += 1,
            None => {}
        }
    }
    let priced = lines
        .iter()
        .all(|line| matches!(line.result, CallResult::Error { .. }) || line.cost_usd.is_some());
    // 每一行都是一次付了錢的呼叫(含後來重做的失敗),全部加起來。
    let cost_usd = priced.then(|| lines.iter().filter_map(|line| line.cost_usd).sum());
    Summary {
        planned: planned.len(),
        ok,
        errors,
        missing: planned.len() - ok - errors,
        cost_usd,
    }
}

/// 開始一次新的執行。
#[allow(clippy::too_many_arguments)]
pub async fn start(
    root: &Path,
    dataset_path: &Path,
    config: RunConfig,
    options: ExecOptions,
    provider: &dyn LlmProvider,
    git_commit: Option<String>,
    now: Timestamp,
) -> Result<(RunDir, Summary), RunError> {
    // 先檢查,再建立任何東西:沒辦法遵守預算就不開始(fail-closed)。
    check_options(&options)?;
    let dataset = Dataset::load(dataset_path)?;
    let meta = RunMeta {
        eval_version: env!("CARGO_PKG_VERSION").into(),
        started_at: now,
        finished_at: None,
        dataset: DatasetInfo {
            name: dataset.name.clone(),
            path: dataset_path.to_path_buf(),
            sha256: dataset.sha256.clone(),
            cases: dataset.cases.len(),
        },
        config,
        git_commit,
        summary: None,
    };
    let dir = RunDir::create(root, &meta)?;
    let summary = execute(&dir, meta, &dataset, options, false, provider, now).await?;
    Ok((dir, summary))
}

/// 接續一次執行:設定從 `meta.json` 讀回來,資料集內容必須沒變。
pub async fn resume(
    run_dir: &Path,
    options: ExecOptions,
    retry_errors: bool,
    provider: &dyn LlmProvider,
    now: Timestamp,
) -> Result<Summary, RunError> {
    check_options(&options)?;
    let dir = RunDir::open(run_dir)?;
    let meta = dir.read_meta()?;
    let dataset = Dataset::load(&meta.dataset.path)?;
    // 內容變了,一次執行裡就會混著兩種題目:拒絕,請開新的執行。
    if dataset.sha256 != meta.dataset.sha256 {
        return Err(RunError::DatasetChanged {
            path: meta.dataset.path.clone(),
            expected: meta.dataset.sha256.clone(),
            found: dataset.sha256,
        });
    }
    execute(&dir, meta, &dataset, options, retry_errors, provider, now).await
}

fn check_options(options: &ExecOptions) -> Result<(), RunError> {
    if options.budget_usd.is_some() && options.price.is_none() {
        return Err(RunError::BudgetWithoutPrice);
    }
    Ok(())
}

/// 做完還缺的工作,更新 `meta.json` 的摘要與結束時間。
async fn execute(
    dir: &RunDir,
    mut meta: RunMeta,
    dataset: &Dataset,
    options: ExecOptions,
    retry_errors: bool,
    provider: &dyn LlmProvider,
    now: Timestamp,
) -> Result<Summary, RunError> {
    let planned = plan(dataset, &meta.config);
    let before = dir.read_responses()?;
    let todo = pending(dataset, &planned, &before, retry_errors);
    // 預算算的是「整次執行」:接續時,之前花的也算進去。
    let spent = Spent::new(before.iter().filter_map(|line| line.cost_usd).sum());

    let mut out = dir.appender()?;
    // `buffer_unordered(n)`:最多同時 n 個 future 在進行,誰先完成就先拿到誰。
    // 這些 future 都在同一個任務裡輪流執行(沒有 spawn),所以可以直接借用 `provider`、`dataset`,不需要 `Arc`。
    // 並行(concurrency)不等於平行(parallelism):等網路回應的時候,其他呼叫可以繼續,這就夠了。
    // Java 對照:像是一個固定大小的連線池 + `CompletableFuture`,但不需要額外的執行緒。
    let model = meta.config.model.as_str();
    let mut results = futures::stream::iter(todo)
        .map(|item| call(item, dataset, model, &options, &spent, provider))
        .buffer_unordered(options.concurrency.max(1));
    // 只有這個迴圈寫檔:一次一行,不需要鎖。
    while let Some(line) = results.next().await {
        if let Some(line) = line {
            out.append(&line)?;
        }
    }
    drop(results);

    let summary = summarize(dataset, &planned, &dir.read_responses()?);
    meta.finished_at = Some(now);
    meta.summary = Some(summary.clone());
    dir.write_meta(&meta)?;
    Ok(summary)
}

/// 做一次呼叫。超過預算就不做,回傳 `None`(那個工作留給接續)。
async fn call(
    item: WorkItem,
    dataset: &Dataset,
    model: &str,
    options: &ExecOptions,
    spent: &Spent,
    provider: &dyn LlmProvider,
) -> Option<ResponseLine> {
    let case = dataset.cases.get(item.case_index)?;
    // 在「開始」時檢查,不是排隊時:已經在進行的照樣做完,所以最多超出(並行數 × 一次呼叫)的費用。
    if let Some(budget) = options.budget_usd
        && spent.usd() >= budget
    {
        return None;
    }
    let request = match &case.input {
        Input::Chat(chat) => chat.to_request(model),
    };
    let started = tokio::time::Instant::now();
    let outcome = tokio::time::timeout(options.timeout, provider.complete(request)).await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (result, cost_usd) = match outcome {
        Ok(Ok(response)) => {
            let cost = options.price.map(|price| cost(&response.usage, price));
            if let Some(cost) = cost {
                spent.add(cost);
            }
            let output = Output::Chat(response);
            (CallResult::Ok { output }, cost)
        }
        Ok(Err(error)) => (
            CallResult::Error {
                error: RecordedError::from(&error),
            },
            None,
        ),
        Err(_) => (
            CallResult::Error {
                error: RecordedError::from(&LlmError::Timeout),
            },
            None,
        ),
    };
    Some(ResponseLine {
        case_id: case.id.clone(),
        sample: item.sample,
        input_sha256: case.input_sha256(),
        result,
        latency_ms,
        cost_usd,
    })
}

/// 累計花費。存成「百萬分之一美元」的整數,才能用 `AtomicU64`(沒有 atomic 的 f64)。
// 為什麼要 atomic:這些 future 雖然在同一個任務裡輪流執行,但整個任務可能被 tokio 搬到別的執行緒,
// 借用的東西必須是 `Sync`。`Cell<u64>` 不是 `Sync`,編譯不會過;`AtomicU64` 是。
pub(crate) struct Spent(AtomicU64);

impl Spent {
    pub(crate) fn new(usd: f64) -> Self {
        Self(AtomicU64::new(to_micros(usd)))
    }

    pub(crate) fn add(&self, usd: f64) {
        self.0.fetch_add(to_micros(usd), Ordering::Relaxed);
    }

    pub(crate) fn usd(&self) -> f64 {
        self.0.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }
}

/// 無條件進位到百萬分之一美元:估高不估低。
fn to_micros(usd: f64) -> u64 {
    // `as u64`:負數與 NaN 會變成 0,太大會停在 u64::MAX(Rust 的 float → int 轉換是飽和的,不會 panic)。
    (usd * 1_000_000.0).ceil() as u64
}

/// 一次呼叫的費用(美元)。cache 讀寫的 token 也按輸入價計(實際比較便宜):估高不估低,預算才守得住。
pub(crate) fn cost(usage: &Usage, price: Price) -> f64 {
    let input = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
    (input as f64 * price.input + usage.output_tokens as f64 * price.output) / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use futures::future::BoxFuture;
    use llm::{Content, EventStream, ProviderMetadata, Request, Response, StopKind, StopReason};

    use super::*;

    /// 假的 provider。題目的文字決定它怎麼回應:
    /// `fail` → 529;`hang` → 永遠不回;`flaky` → `flaky_fails` 為 true 時 529;其他 → 回答。
    /// 每次呼叫用掉 1000 個輸入、500 個輸出 token。
    #[derive(Default)]
    struct Fake {
        calls: AtomicUsize,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        flaky_fails: AtomicBool,
    }

    impl Fake {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    fn prompt(request: &Request) -> String {
        match request.messages.first().and_then(|m| m.content.first()) {
            Some(Content::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    impl LlmProvider for Fake {
        fn stream(&self, _request: Request) -> EventStream {
            Box::pin(futures::stream::empty())
        }

        fn complete(&self, request: Request) -> BoxFuture<'_, llm::Result<Response>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                // 讓呼叫真的「進行中」一段時間,並行才看得出來(時間是暫停的,不會真的等)。
                tokio::time::sleep(Duration::from_millis(10)).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                let text = prompt(&request);
                if text == "fail" || (text == "flaky" && self.flaky_fails.load(Ordering::SeqCst)) {
                    return Err(LlmError::http(529, None, ""));
                }
                if text == "hang" {
                    return futures::future::pending().await;
                }
                Ok(Response {
                    id: None,
                    model: format!("{}-id", request.model),
                    content: vec![Content::Text {
                        text: format!("answer to {text}"),
                        meta: ProviderMetadata::default(),
                    }],
                    stop: StopReason {
                        kind: StopKind::EndTurn,
                        raw: "end_turn".into(),
                    },
                    usage: Usage {
                        input_tokens: 1000,
                        output_tokens: 500,
                        ..Usage::default()
                    },
                    meta: ProviderMetadata::default(),
                })
            })
        }
    }

    /// 每次呼叫:1000 × $2 + 500 × $10(每百萬 token)= $0.007。
    const PRICE: Price = Price {
        input: 2.0,
        output: 10.0,
    };
    const PER_CALL: f64 = 0.007;

    fn line(id: &str, text: &str, tags: &[&str]) -> String {
        format!(
            r#"{{"id":"{id}","tags":{},"input":{{"type":"chat","max_tokens":64,"messages":[{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}]}},"checks":[{{"type":"contains","value":"x"}}]}}"#,
            serde_json::to_string(tags).unwrap()
        )
    }

    /// 在暫存目錄寫一個資料集檔案,回傳 (暫存目錄, 資料集路徑, runs 目錄)。
    fn setup(lines: &[String]) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ds.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        let runs = tmp.path().join("runs");
        (tmp, path, runs)
    }

    fn config(repeats: u32) -> RunConfig {
        RunConfig {
            model: "cheap".into(),
            repeats,
            tags: vec![],
            limit: None,
            label: None,
        }
    }

    fn options(concurrency: usize) -> ExecOptions {
        ExecOptions {
            concurrency,
            timeout: Duration::from_secs(300),
            budget_usd: None,
            price: Some(PRICE),
        }
    }

    fn t(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn keys(dataset: &Dataset, items: &[WorkItem]) -> Vec<(String, u32)> {
        items
            .iter()
            .map(|w| (dataset.cases[w.case_index].id.clone(), w.sample))
            .collect()
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn plan_filters_by_tags_then_limits_then_expands_samples() {
        let ds = Dataset::parse(
            "ds",
            [
                line("a", "q", &["rust"]),
                line("b", "q", &["go"]),
                line("c", "q", &["rust", "web"]),
            ]
            .join("\n")
            .as_bytes(),
        )
        .unwrap();
        let k = |c: &RunConfig| keys(&ds, &plan(&ds, c));
        let s = |v: &str, n: u32| (v.to_owned(), n);

        assert_eq!(k(&config(1)), [s("a", 0), s("b", 0), s("c", 0)]);
        // 帶有其中「任一個」tag 就算。
        let any = RunConfig {
            tags: vec!["web".into(), "go".into()],
            ..config(1)
        };
        assert_eq!(k(&any), [s("b", 0), s("c", 0)]);
        // 先篩選,再取前幾個,再展開成多次抽樣。
        let limited = RunConfig {
            tags: vec!["rust".into()],
            limit: Some(1),
            ..config(2)
        };
        assert_eq!(k(&limited), [s("a", 0), s("a", 1)]);
    }

    #[test]
    fn the_estimate_assumes_every_answer_uses_all_of_max_tokens() {
        let ds = Dataset::parse("ds", line("a", "q", &[]).as_bytes()).unwrap();
        let items = plan(&ds, &config(3));
        let Input::Chat(input) = &ds.cases[0].input;
        // 輸入:請求 JSON 的 bytes ÷ 2 當 token 數(刻意高估);輸出:max_tokens(64)全部用完。
        let input_tokens = serde_json::to_vec(&input.to_request("cheap"))
            .unwrap()
            .len() as f64
            / 2.0;
        let per_call = (input_tokens * PRICE.input + 64.0 * PRICE.output) / 1_000_000.0;

        let e = estimate(&ds, &items, Some(PRICE));

        assert_eq!(e.calls, 3);
        assert!(approx(e.worst_case_usd.unwrap(), 3.0 * per_call), "{e:?}");
        assert_eq!(estimate(&ds, &items, None).worst_case_usd, None);
    }

    #[tokio::test(start_paused = true)]
    async fn start_records_every_answer_with_its_cost() {
        let (_tmp, path, runs) = setup(&[line("a", "q1", &[]), line("b", "q2", &[])]);
        let fake = Fake::default();

        let (dir, summary) = start(
            &runs,
            &path,
            config(2),
            options(2),
            &fake,
            Some("abc123".into()),
            t("2026-11-01T12:00:00Z"),
        )
        .await
        .unwrap();

        assert_eq!(fake.calls(), 4);
        assert_eq!(
            (summary.planned, summary.ok, summary.errors, summary.missing),
            (4, 4, 0, 0)
        );
        assert!(
            approx(summary.cost_usd.unwrap(), 4.0 * PER_CALL),
            "{summary:?}"
        );

        let lines = dir.read_responses().unwrap();
        assert_eq!(lines.len(), 4);
        let first = lines.iter().find(|l| l.case_id == "a").unwrap();
        assert!(approx(first.cost_usd.unwrap(), PER_CALL));
        let CallResult::Ok { output } = &first.result else {
            panic!("{first:?}")
        };
        // 送出去的模型是設定裡的名字;Alias 換成真的 id 之後,記在回應裡。
        assert_eq!(output.text(), "answer to q1");
        // 每一筆都記下自己那題輸入的 hash(打分時確認題目沒變)。
        let ds = Dataset::load(&path).unwrap();
        for l in &lines {
            let case = ds.cases.iter().find(|c| c.id == l.case_id).unwrap();
            assert_eq!(l.input_sha256, case.input_sha256(), "{}", l.case_id);
        }
        assert_ne!(lines[0].input_sha256, ds.cases[1].input_sha256());

        let meta = dir.read_meta().unwrap();
        assert_eq!(meta.config, config(2));
        assert_eq!(meta.dataset.name, "ds");
        assert_eq!(meta.dataset.cases, 2);
        assert_eq!(meta.git_commit.as_deref(), Some("abc123"));
        assert_eq!(meta.summary, Some(summary));
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_is_capped_and_actually_used() {
        let (_tmp, path, runs) = setup(&[line("a", "q", &[])]);
        let fake = Fake::default();

        start(
            &runs,
            &path,
            config(6),
            options(3),
            &fake,
            None,
            t("2026-11-01T12:00:00Z"),
        )
        .await
        .unwrap();

        // 剛好 3:不超過上限,也不是一個一個來。
        assert_eq!(fake.max_in_flight.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_and_timeouts_are_recorded_not_skipped() {
        let (_tmp, path, runs) = setup(&[line("a", "fail", &[]), line("b", "hang", &[])]);
        let fake = Fake::default();
        let opts = ExecOptions {
            timeout: Duration::from_secs(5),
            ..options(2)
        };

        let (dir, summary) = start(
            &runs,
            &path,
            config(1),
            opts,
            &fake,
            None,
            t("2026-11-01T12:00:00Z"),
        )
        .await
        .unwrap();

        assert_eq!((summary.ok, summary.errors, summary.missing), (0, 2, 0));
        let mut lines = dir.read_responses().unwrap();
        lines.sort_by(|x, y| x.case_id.cmp(&y.case_id));
        let errors: Vec<(&str, Option<u16>)> = lines
            .iter()
            .map(|l| match &l.result {
                CallResult::Error { error } => (error.kind.as_str(), error.status),
                CallResult::Ok { .. } => ("ok", None),
            })
            .collect();
        assert_eq!(errors, [("http", Some(529)), ("timeout", None)]);
        // 逾時的那一筆:延遲就是上限,而且沒有用量、沒有費用。
        assert_eq!(lines[1].latency_ms, 5_000);
        assert_eq!(lines[1].cost_usd, None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_budget_stops_new_calls_and_resume_finishes_the_rest() {
        let (_tmp, path, runs) = setup(&[line("a", "q", &[])]);
        let fake = Fake::default();
        // 一個一個來:花了 0、0.007、0.014 時都還沒到 0.015,所以做了 3 次;0.021 之後就不再送出。
        let opts = ExecOptions {
            budget_usd: Some(0.015),
            ..options(1)
        };

        let (dir, first) = start(
            &runs,
            &path,
            config(5),
            opts,
            &fake,
            None,
            t("2026-11-01T12:00:00Z"),
        )
        .await
        .unwrap();

        assert_eq!(fake.calls(), 3);
        assert_eq!((first.ok, first.missing), (3, 2));

        // 預算算的是整次執行:之前已經花了 0.021,上限 0.025 只夠再開始 1 次(0.021 < 0.025,做完 0.028 就停)。
        // 如果只算這次接續花的,會做 2 次。
        let tight = ExecOptions {
            budget_usd: Some(0.025),
            ..options(1)
        };
        let middle = resume(dir.path(), tight, false, &fake, t("2026-11-01T12:05:00Z"))
            .await
            .unwrap();
        assert_eq!(fake.calls(), 4);
        assert_eq!((middle.ok, middle.missing), (4, 1));

        // 調高預算接續:只補缺的那 1 次,前面的不重付。
        let more = ExecOptions {
            budget_usd: Some(1.0),
            ..options(1)
        };
        let second = resume(dir.path(), more, false, &fake, t("2026-11-01T12:10:00Z"))
            .await
            .unwrap();

        assert_eq!(fake.calls(), 5);
        assert_eq!((second.ok, second.missing), (5, 0));
        assert!(
            approx(second.cost_usd.unwrap(), 5.0 * PER_CALL),
            "{second:?}"
        );
        let meta = dir.read_meta().unwrap();
        assert_eq!(meta.finished_at, Some(t("2026-11-01T12:10:00Z")));
        assert_eq!(meta.started_at, t("2026-11-01T12:00:00Z"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_budget_without_a_price_is_refused_before_anything_happens() {
        let (_tmp, path, runs) = setup(&[line("a", "q", &[])]);
        let fake = Fake::default();
        let opts = ExecOptions {
            budget_usd: Some(1.0),
            price: None,
            ..options(1)
        };

        let err = start(
            &runs,
            &path,
            config(1),
            opts,
            &fake,
            None,
            t("2026-11-01T12:00:00Z"),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, RunError::BudgetWithoutPrice), "{err:?}");
        assert_eq!(fake.calls(), 0);
        // 連執行目錄都還沒建立:沒辦法遵守上限,就不開始。
        assert!(!runs.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn resume_refuses_a_dataset_that_changed() {
        let (_tmp, path, runs) = setup(&[line("a", "q", &[])]);
        let fake = Fake::default();
        let opts = ExecOptions {
            budget_usd: Some(0.001),
            ..options(1)
        };
        let (dir, _) = start(
            &runs,
            &path,
            config(3),
            opts,
            &fake,
            None,
            t("2026-11-01T12:00:00Z"),
        )
        .await
        .unwrap();
        std::fs::write(&path, [line("a", "q", &[]), line("b", "q", &[])].join("\n")).unwrap();

        let err = resume(
            dir.path(),
            options(1),
            false,
            &fake,
            t("2026-11-01T12:10:00Z"),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&err, RunError::DatasetChanged { path: p, .. } if *p == path),
            "{err:?}"
        );
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_calls_are_redone_only_when_asked_and_the_last_line_wins() {
        let (_tmp, path, runs) = setup(&[line("a", "q", &[]), line("b", "flaky", &[])]);
        let fake = Fake::default();
        fake.flaky_fails.store(true, Ordering::SeqCst);
        let (dir, first) = start(
            &runs,
            &path,
            config(1),
            options(1),
            &fake,
            None,
            t("2026-11-01T12:00:00Z"),
        )
        .await
        .unwrap();
        assert_eq!((first.ok, first.errors), (1, 1));

        // 預設:失敗也算做過,不重做(結果才可重現)。
        let same = resume(
            dir.path(),
            options(1),
            false,
            &fake,
            t("2026-11-01T12:01:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(fake.calls(), 2);
        assert_eq!((same.ok, same.errors), (1, 1));

        // `--retry-errors`:只重做失敗的那一個;新的結果加在最後,以它為準。
        fake.flaky_fails.store(false, Ordering::SeqCst);
        let retried = resume(
            dir.path(),
            options(1),
            true,
            &fake,
            t("2026-11-01T12:02:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(fake.calls(), 3);
        assert_eq!((retried.ok, retried.errors, retried.missing), (2, 0, 0));
        assert_eq!(dir.read_responses().unwrap().len(), 3);
    }

    #[test]
    fn cache_tokens_are_charged_at_the_input_price() {
        // 估高不估低:cache 讀取其實比較便宜,但這裡照輸入價算,預算才守得住。
        // (cargo-mutants:假 provider 的 cache token 都是 0,`+` 換成 `-`、`*` 時原本沒有測試失敗。)
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 300,
            cache_write_tokens: 50,
        };
        // (100 + 300 + 50) × $2 + 10 × $10 = 900 + 100 = $1000 / 百萬
        assert!(approx(cost(&usage, PRICE), 0.001));
    }

    #[test]
    fn the_cost_is_unknown_unless_every_answer_has_one() {
        let ds = Dataset::parse(
            "ds",
            [line("a", "q", &[]), line("b", "q", &[])]
                .join("\n")
                .as_bytes(),
        )
        .unwrap();
        let items = plan(&ds, &config(1));
        let answered = |id: &str, cost: Option<f64>| ResponseLine {
            case_id: id.into(),
            sample: 0,
            input_sha256: String::new(),
            result: CallResult::Error {
                error: RecordedError {
                    kind: "http".into(),
                    status: Some(529),
                    message: String::new(),
                },
            },
            latency_ms: 1,
            cost_usd: cost,
        };
        // 失敗的呼叫沒有費用(算 0),總額照樣算得出來。
        let s = summarize(&ds, &items, &[answered("a", None)]);
        assert_eq!((s.errors, s.missing, s.cost_usd), (1, 1, Some(0.0)));
    }
}
