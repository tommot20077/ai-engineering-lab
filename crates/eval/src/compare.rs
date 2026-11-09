//! 比較兩次執行(design §7、D-032、D-035)。
//!
//! ```text
//! 兩個執行目錄 + 同名的打分 ──load──→ RunScores × 2
//!   ──打分設定不同?──→ 拒絕(--allow-mismatch:照樣比,報告最上面標示)
//!   ──stats──→ 兩種指標、配對、bootstrap 區間、翻轉的案例、依 tag 分組、每條標準、成本、延遲
//!   ──Display──→ 給人看的報告
//! ```
//!
//! 讀取路徑 fail-closed:不公平的比較比沒有結果更糟,會讓人做出錯的決定。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::dataset::Dataset;
use crate::run::RunConfig;
use crate::score::flow::{current_scores, differences, latest};
use crate::score::{ScoreLine, ScoreMeta};
use crate::stats::{self, Interval, Measure, Pairing, Stats};
use crate::store::{CallResult, ResponseLine, RunDir, RunMeta, StoreError};

/// 一次執行與它的一份打分,讀進來、只留「目前有效」的部分。
#[derive(Debug, Clone)]
pub struct RunScores {
    pub path: PathBuf,
    pub run: RunMeta,
    /// 每個 (案例, 抽樣) 最後一筆回應。
    pub responses: Vec<ResponseLine>,
    pub score_name: String,
    pub scores: ScoreMeta,
    /// 評的是最新回應的那些打分(舊的、作廢的不算)。
    pub lines: Vec<ScoreLine>,
    /// 案例 id → tag,從執行記錄的資料集路徑讀。讀不到時是空的,原因在 `tags_unavailable`。
    pub tags: BTreeMap<String, Vec<String>>,
    pub tags_unavailable: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CompareError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("{} has no score named `{name}`; run `eval score` first", path.display())]
    ScoresMissing { path: PathBuf, name: String },

    #[error(
        "the two runs have no scoring in common (A has: {}; B has: {}); score both with the same --name first",
        list_or_none(a),
        list_or_none(b)
    )]
    NoCommonScores { a: Vec<String>, b: Vec<String> },

    #[error("both runs have several scorings ({}); pick one with --scores", common.join(", "))]
    AmbiguousScores { common: Vec<String> },

    #[error(
        "the two scores were made with different settings, so they are not comparable:\n{}\npass --allow-mismatch to compare anyway",
        differences.iter().map(|d| format!("  - {d}")).collect::<Vec<_>>().join("\n")
    )]
    Mismatch { differences: Vec<String> },
}

/// 比較報告。`Display` 印出給人看的版本。
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub a: Side,
    pub b: Side,
    pub score_name: String,
    pub scores: crate::score::ScoreConfig,
    /// 打分設定的差異。只有 `--allow-mismatch` 時才可能不是空的;報告最上面標示「不可直接比較」。
    pub mismatches: Vec<String>,
    /// 兩邊的設定完全相同:這是雜訊底線(A/A),不是結果(D-035)。
    pub noise_floor: bool,
    pub warnings: Vec<CompareWarning>,
    pub all_pass: Comparison,
    pub partial: Comparison,
    /// 全部通過率變差的案例,變化大的在前。人最需要看的就是這些。
    pub regressed: Vec<Flip>,
    pub improved: Vec<Flip>,
    pub by_tag: Vec<TagRow>,
    /// 兩邊都有、而且通過率不同的標準。
    pub criteria: Vec<CriterionRow>,
}

/// 一邊的摘要。
#[derive(Debug, Clone, PartialEq)]
pub struct Side {
    pub path: PathBuf,
    /// 送出去的名字(通常是別名)與實際回答的模型 id。
    pub model: String,
    pub served: Vec<String>,
    pub label: Option<String>,
    pub stats: Stats,
    pub run_cost_usd: Option<f64>,
    pub score_cost_usd: Option<f64>,
    /// 成功的呼叫的延遲(毫秒)。失敗的(例如逾時)不算,錯誤率另外列。
    pub latency_p50_ms: Option<u64>,
    pub latency_p95_ms: Option<u64>,
}

impl Side {
    /// 執行 + 打分的總費用。任何一邊未知,總額就是未知(不能當成 0)。
    pub fn total_cost_usd(&self) -> Option<f64> {
        // `?` 用在 Option 上:任何一個是 `None` 就直接回傳 `None`。Java 沒有對應的語法,要寫兩個 null 檢查。
        Some(self.run_cost_usd? + self.score_cost_usd?)
    }

    pub fn cost_per_case_usd(&self) -> Option<f64> {
        let cases = self.stats.cases;
        (cases > 0).then_some(self.total_cost_usd()? / cases as f64)
    }
}

/// 一個指標的比較。
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    pub pairing: Pairing,
    /// 「B − A」的 95% 信賴區間;沒有可以配對的案例時是 `None`。
    pub interval: Option<Interval>,
}

impl Comparison {
    /// 配對案例上兩邊的平均。報告的 A、B 兩欄用它:跟差值、區間是同一批案例,數字才對得起來
    /// (各自整次執行的平均可能多了只在一邊的案例,見 `Side::stats`)。
    pub fn paired_means(&self) -> (Option<f64>, Option<f64>) {
        let pairs = &self.pairing.pairs;
        let mean = |pick: fn(&(String, f64, f64)) -> f64| {
            (!pairs.is_empty()).then(|| pairs.iter().map(pick).sum::<f64>() / pairs.len() as f64)
        };
        (mean(|p| p.1), mean(|p| p.2))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flip {
    pub case_id: String,
    pub a: f64,
    pub b: f64,
}

/// 一個 tag 底下的案例,兩邊的平均。只列平均:每個 tag 的題目通常很少,算區間意義不大。
#[derive(Debug, Clone, PartialEq)]
pub struct TagRow {
    pub tag: String,
    pub all_pass: Means,
    pub partial: Means,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Means {
    pub cases: usize,
    pub a: Option<f64>,
    pub b: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CriterionRow {
    pub case_id: String,
    pub scorer: String,
    pub criterion: String,
    pub a: f64,
    pub b: f64,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CompareWarning {
    #[error(
        "the two runs used different dataset contents (sha256 {a} vs {b}); only cases in both are compared"
    )]
    DatasetChanged { a: String, b: String },

    #[error(
        "run {side} is incomplete: {missing} call(s) never ran (budget or interruption); resume it first"
    )]
    RunIncomplete { side: char, missing: usize },

    #[error(
        "the scores of run {side} are incomplete: {missing} response(s) not scored, {incomplete} with scorer errors"
    )]
    ScoresIncomplete {
        side: char,
        missing: usize,
        incomplete: usize,
    },

    #[error("only {cases} paired case(s): the interval is wide; aim for 30 or more")]
    FewCases { cases: usize },

    #[error("tags are not shown: the dataset could not be read ({reason})")]
    TagsUnavailable { reason: String },
}

/// 沒指定要比哪一份打分時:兩邊都有的那一份。沒有、或不只一份,就列出來請使用者指定(不猜)。
pub fn common_score_name(a: &Path, b: &Path) -> Result<String, CompareError> {
    let names_a = RunDir::open(a)?.score_names()?;
    let names_b = RunDir::open(b)?.score_names()?;
    let common: Vec<String> = names_a
        .iter()
        .filter(|name| names_b.contains(name))
        .cloned()
        .collect();
    // slice pattern:`[only]` 只在剛好一個元素時符合。Java 對照:`if (list.size() == 1) list.get(0)`,但不會拿錯索引。
    match common.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(CompareError::NoCommonScores {
            a: names_a,
            b: names_b,
        }),
        _ => Err(CompareError::AmbiguousScores { common }),
    }
}

/// 讀一次執行與它名為 `name` 的打分。
pub fn load(run_dir: &Path, name: &str) -> Result<RunScores, CompareError> {
    let dir = RunDir::open(run_dir)?;
    let run = dir.read_meta()?;
    let all = dir.read_responses()?;
    let files = dir.scores(name)?;
    let scores: ScoreMeta = files
        .read_meta()?
        .ok_or_else(|| CompareError::ScoresMissing {
            path: run_dir.to_path_buf(),
            name: name.to_owned(),
        })?;
    let all_lines: Vec<ScoreLine> = files.read_lines()?;

    // 只留評的是「最新那筆回應」的打分(同 `score::score` 的摘要):回應重做過的,舊打分作廢。
    let current = current_scores(&all_lines);
    let indexes = latest(&all);
    let lines = indexes
        .iter()
        .filter_map(|&i| {
            let response = all.get(i)?;
            let score = current.get(&(response.case_id.as_str(), response.sample))?;
            (score.response == i).then(|| (*score).clone())
        })
        .collect();
    let responses = indexes
        .iter()
        .filter_map(|&i| all.get(i).cloned())
        .collect();

    // tag 不在打分檔裡,從資料集讀。讀不到不是錯誤:結果照樣比較,只是不分組(並警告)。
    let (tags, tags_unavailable) = match Dataset::load(&run.dataset.path) {
        Ok(dataset) => (
            dataset
                .cases
                .into_iter()
                .map(|case| (case.id, case.tags))
                .collect(),
            None,
        ),
        Err(e) => (BTreeMap::new(), Some(e.to_string())),
    };

    Ok(RunScores {
        path: run_dir.to_path_buf(),
        run,
        responses,
        score_name: name.to_owned(),
        scores,
        lines,
        tags,
        tags_unavailable,
    })
}

/// 比較兩份打分。打分設定不同時拒絕,除非 `allow_mismatch`。
pub fn compare(a: &RunScores, b: &RunScores, allow_mismatch: bool) -> Result<Report, CompareError> {
    // 打分設定不同,分數的意思就不同(換了評審、改了標準):預設拒絕(D-035)。
    let mut mismatches = differences(&a.scores.config, &b.scores.config);
    if a.scores.eval_version != b.scores.eval_version {
        mismatches.push(format!(
            "eval version: {} → {}",
            a.scores.eval_version, b.scores.eval_version
        ));
    }
    if !mismatches.is_empty() && !allow_mismatch {
        return Err(CompareError::Mismatch {
            differences: mismatches,
        });
    }

    let (cases_a, cases_b) = (stats::case_scores(&a.lines), stats::case_scores(&b.lines));
    let comparison = |measure| {
        let pairing = stats::pair(&cases_a, &cases_b, measure);
        let interval = stats::paired_bootstrap(&pairing.values(), stats::RESAMPLES, stats::SEED);
        Comparison { pairing, interval }
    };
    let all_pass = comparison(Measure::AllPass);
    let partial = comparison(Measure::Partial);
    let (side_a, side_b) = (side(a), side(b));

    let noise_floor = mismatches.is_empty() && same_run(a, b) && side_a.served == side_b.served;
    let warnings = warnings(a, b, &all_pass);
    let (regressed, improved) = flips(&all_pass.pairing);
    let by_tag = by_tag(a, b, &all_pass.pairing, &partial.pairing);
    let criteria = changed_criteria(&side_a.stats, &side_b.stats);

    Ok(Report {
        a: side_a,
        b: side_b,
        score_name: a.score_name.clone(),
        scores: a.scores.config.clone(),
        mismatches,
        noise_floor,
        warnings,
        all_pass,
        partial,
        regressed,
        improved,
        by_tag,
        criteria,
    })
}

/// 執行設定相同:模型、重複次數、篩選條件、資料集內容。`label` 只是說明文字,不算。
fn same_run(a: &RunScores, b: &RunScores) -> bool {
    let without_label = |c: &RunConfig| RunConfig {
        label: None,
        ..c.clone()
    };
    without_label(&a.run.config) == without_label(&b.run.config)
        && a.run.dataset.sha256 == b.run.dataset.sha256
}

fn side(r: &RunScores) -> Side {
    let served: BTreeSet<&str> = r
        .responses
        .iter()
        .filter_map(|line| match &line.result {
            CallResult::Ok { output } => Some(output.model()),
            CallResult::Error { .. } => None,
        })
        .collect();
    let mut latencies: Vec<u64> = r
        .responses
        .iter()
        .filter(|line| matches!(line.result, CallResult::Ok { .. }))
        .map(|line| line.latency_ms)
        .collect();
    latencies.sort_unstable();
    Side {
        path: r.path.clone(),
        model: r.run.config.model.clone(),
        served: served.into_iter().map(str::to_owned).collect(),
        label: r.run.config.label.clone(),
        stats: stats::stats(&r.lines),
        run_cost_usd: r.run.summary.as_ref().and_then(|s| s.cost_usd),
        score_cost_usd: r.scores.summary.as_ref().and_then(|s| s.cost_usd),
        latency_p50_ms: percentile(&latencies, 50),
        latency_p95_ms: percentile(&latencies, 95),
    }
}

fn warnings(a: &RunScores, b: &RunScores, all_pass: &Comparison) -> Vec<CompareWarning> {
    let mut out = Vec::new();
    if a.run.dataset.sha256 != b.run.dataset.sha256 {
        out.push(CompareWarning::DatasetChanged {
            a: a.run.dataset.sha256.clone(),
            b: b.run.dataset.sha256.clone(),
        });
    }
    for (side, r) in [('A', a), ('B', b)] {
        if let Some(summary) = &r.run.summary
            && summary.missing > 0
        {
            out.push(CompareWarning::RunIncomplete {
                side,
                missing: summary.missing,
            });
        }
    }
    for (side, r) in [('A', a), ('B', b)] {
        if let Some(summary) = &r.scores.summary
            && (summary.missing > 0 || summary.incomplete > 0)
        {
            out.push(CompareWarning::ScoresIncomplete {
                side,
                missing: summary.missing,
                incomplete: summary.incomplete,
            });
        }
    }
    let cases = all_pass.pairing.pairs.len();
    if cases < stats::FEW_CASES {
        out.push(CompareWarning::FewCases { cases });
    }
    if let Some(reason) = a.tags_unavailable.as_ref().or(b.tags_unavailable.as_ref()) {
        out.push(CompareWarning::TagsUnavailable {
            reason: reason.clone(),
        });
    }
    out
}

/// 全部通過率有變的案例:(變差的, 變好的),各自依變化幅度由大到小,一樣大的依 id。
fn flips(pairing: &Pairing) -> (Vec<Flip>, Vec<Flip>) {
    let flip = |(id, a, b): &(String, f64, f64)| Flip {
        case_id: id.clone(),
        a: *a,
        b: *b,
    };
    let mut regressed: Vec<Flip> = pairing
        .pairs
        .iter()
        .filter(|(_, a, b)| b < a)
        .map(flip)
        .collect();
    let mut improved: Vec<Flip> = pairing
        .pairs
        .iter()
        .filter(|(_, a, b)| b > a)
        .map(flip)
        .collect();
    // `pairs` 本來就依 id 排序;`sort_by` 是穩定排序,幅度一樣的保持 id 順序。
    let by_change = |x: &Flip, y: &Flip| (y.b - y.a).abs().total_cmp(&(x.b - x.a).abs());
    regressed.sort_by(by_change);
    improved.sort_by(by_change);
    (regressed, improved)
}

/// 依 tag 分組的平均。tag 以 A 的資料集為準,A 沒有的案例再看 B。
fn by_tag(a: &RunScores, b: &RunScores, all_pass: &Pairing, partial: &Pairing) -> Vec<TagRow> {
    let tags_of = |id: &str| a.tags.get(id).or_else(|| b.tags.get(id));
    let tags: BTreeSet<&String> = a.tags.values().chain(b.tags.values()).flatten().collect();
    let means = |pairing: &Pairing, tag: &str| {
        let rows: Vec<(f64, f64)> = pairing
            .pairs
            .iter()
            .filter(|(id, _, _)| tags_of(id).is_some_and(|t| t.iter().any(|x| x == tag)))
            .map(|(_, x, y)| (*x, *y))
            .collect();
        let mean = |values: Vec<f64>| {
            (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
        };
        Means {
            cases: rows.len(),
            a: mean(rows.iter().map(|r| r.0).collect()),
            b: mean(rows.iter().map(|r| r.1).collect()),
        }
    };
    tags.into_iter()
        .map(|tag| TagRow {
            tag: tag.clone(),
            all_pass: means(all_pass, tag),
            partial: means(partial, tag),
        })
        .collect()
}

/// 兩邊都有、通過率不同的標準。
fn changed_criteria(a: &Stats, b: &Stats) -> Vec<CriterionRow> {
    let rates = |s: &Stats| -> BTreeMap<(String, String, String), Option<f64>> {
        s.criteria
            .iter()
            .map(|c| {
                (
                    (c.case_id.clone(), c.scorer.clone(), c.criterion.clone()),
                    c.rate(),
                )
            })
            .collect()
    };
    let (ra, rb) = (rates(a), rates(b));
    ra.into_iter()
        .filter_map(|(key, x)| {
            let (x, y) = (x?, (*rb.get(&key)?)?);
            let (case_id, scorer, criterion) = key;
            (x != y).then_some(CriterionRow {
                case_id,
                scorer,
                criterion,
                a: x,
                b: y,
            })
        })
        .collect()
}

/// 第 `p` 百分位(nearest-rank):排序後第 ⌈p × n / 100⌉ 個。沒有資料時是 `None`。
fn percentile(sorted: &[u64], p: usize) -> Option<u64> {
    // 整數運算的無條件進位:(p × n + 99) / 100。第 k 個(從 1 數)的索引是 k − 1。
    let rank = (p * sorted.len()).div_ceil(100);
    sorted.get(rank.checked_sub(1)?).copied()
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `writeln!` 對 `Formatter` 寫一行,`?` 把寫入錯誤往外傳。Java 對照:`toString()` 裡用 `StringBuilder`,
        // 但這裡直接寫進呼叫者給的輸出(檔案、終端機),不先組出整個字串。
        writeln!(f, "Compare")?;
        for (name, side) in [("A", &self.a), ("B", &self.b)] {
            write!(
                f,
                "  {name}  {} ({})",
                side.model,
                list_or_dash(&side.served)
            )?;
            if let Some(label) = &side.label {
                write!(f, "  label: {label}")?;
            }
            writeln!(f, "\n     {}", side.path.display())?;
        }
        let judge = match &self.scores.judge {
            Some(j) => format!("judge {} (effort {})", j.model, j.effort),
            None => "checks only".to_owned(),
        };
        let skip = if self.scores.skip_on_check_failure {
            ", judge skipped when a check fails"
        } else {
            ", judge on every answer"
        };
        writeln!(f, "  scores `{}`: {judge}{skip}", self.score_name)?;

        if !self.mismatches.is_empty() {
            writeln!(
                f,
                "\nNOT DIRECTLY COMPARABLE: the scores were made with different settings:"
            )?;
            for m in &self.mismatches {
                writeln!(f, "  - {m}")?;
            }
        }
        if self.noise_floor {
            writeln!(
                f,
                "\nNOISE FLOOR: both runs used identical settings, so any difference below is measurement noise, not a result."
            )?;
        }
        if !self.warnings.is_empty() {
            writeln!(f)?;
            for w in &self.warnings {
                writeln!(f, "warning: {w}")?;
            }
        }

        // `{:<14}`:靠左、寬 14;`{:>8}`:靠右、寬 8。Java 對照:`String.format("%-14s%8s", ...)`。
        writeln!(
            f,
            "\n{:<14}{:>8}{:>8}{:>10}  {:<18}{:>4}",
            "", "A", "B", "B - A", "95% CI", "n"
        )?;
        for (name, c) in [("all-pass", &self.all_pass), ("partial", &self.partial)] {
            let (a, b) = c.paired_means();
            write!(f, "{name:<14}{:>8}{:>8}", pct(a), pct(b))?;
            match &c.interval {
                Some(i) => writeln!(
                    f,
                    "{:>10}  {:<18}{:>4}  {}",
                    points(i.diff),
                    format!("[{}, {}]", points(i.low), points(i.high)),
                    i.cases,
                    verdict(i)
                )?,
                None => writeln!(f, "  no paired cases")?,
            }
        }
        writeln!(
            f,
            "(A and B: means over the n paired cases; paired bootstrap, {} resamples, seed {:#x})",
            stats::RESAMPLES,
            stats::SEED
        )?;

        let pairing = &self.all_pass.pairing;
        for (title, ids) in [
            ("only in A", &pairing.only_a),
            ("only in B", &pairing.only_b),
            ("not scorable on both sides", &pairing.unscored),
        ] {
            if !ids.is_empty() {
                writeln!(f, "{title}: {}", ids.join(", "))?;
            }
        }

        for (title, flips) in [
            ("regressed (A -> B)", &self.regressed),
            ("improved (A -> B)", &self.improved),
        ] {
            if !flips.is_empty() {
                writeln!(f, "\n{title}:")?;
                for flip in flips {
                    writeln!(
                        f,
                        "  {:<24}{:>8} -> {}",
                        flip.case_id,
                        pct(Some(flip.a)),
                        pct(Some(flip.b))
                    )?;
                }
            }
        }

        if !self.by_tag.is_empty() {
            writeln!(f, "\nby tag (A -> B):")?;
            for row in &self.by_tag {
                writeln!(
                    f,
                    "  {:<16}n={:<4}all-pass {} -> {}   partial {} -> {}",
                    row.tag,
                    row.all_pass.cases,
                    pct(row.all_pass.a),
                    pct(row.all_pass.b),
                    pct(row.partial.a),
                    pct(row.partial.b)
                )?;
            }
        }

        if !self.criteria.is_empty() {
            writeln!(f, "\ncriteria that changed (A -> B):")?;
            for c in &self.criteria {
                writeln!(
                    f,
                    "  {}  {}: {}   {} -> {}",
                    c.case_id,
                    c.scorer,
                    c.criterion,
                    pct(Some(c.a)),
                    pct(Some(c.b))
                )?;
            }
        }

        writeln!(f, "\n{:<18}{:<16}B", "", "A")?;
        let row = |f: &mut fmt::Formatter<'_>, name: &str, a: String, b: String| {
            writeln!(f, "{name:<18}{a:<16}{b}")
        };
        row(
            f,
            "cost",
            usd(self.a.total_cost_usd()),
            usd(self.b.total_cost_usd()),
        )?;
        row(
            f,
            "  per case",
            usd(self.a.cost_per_case_usd()),
            usd(self.b.cost_per_case_usd()),
        )?;
        row(f, "latency p50/p95", latency(&self.a), latency(&self.b))?;
        row(
            f,
            "call errors",
            errors(&self.a.stats),
            errors(&self.b.stats),
        )?;
        row(
            f,
            "incomplete",
            self.a.stats.incomplete.to_string(),
            self.b.stats.incomplete.to_string(),
        )
    }
}

/// 看區間落在 0 的哪一邊,不看觀察到的差:「有沒有差異」本來就是區間決定的。
/// (cargo-mutants:原本用 `diff > 0.0` 分好壞,換成 `>=` 時結果一樣,因為區間不含 0 時 diff 不會剛好是 0。)
fn verdict(i: &Interval) -> &'static str {
    if i.low > 0.0 {
        "B is better"
    } else if i.high < 0.0 {
        "B is worse"
    } else {
        "no detectable difference"
    }
}

/// 比例 → `62.5%`;沒有資料 → `-`。
fn pct(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{:.1}%", v * 100.0))
}

/// 比例的差 → 百分點,帶正負號:`+8.0`。`{:+.1}`:一定印出正負號、小數一位。
fn points(value: f64) -> String {
    format!("{:+.1}", value * 100.0)
}

fn usd(value: Option<f64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |v| format!("${v:.4}"))
}

fn latency(side: &Side) -> String {
    match (side.latency_p50_ms, side.latency_p95_ms) {
        (Some(p50), Some(p95)) => format!("{p50}/{p95} ms"),
        _ => "-".to_owned(),
    }
}

fn errors(stats: &Stats) -> String {
    let rate = (stats.responses > 0).then(|| stats.call_errors as f64 / stats.responses as f64);
    format!("{}/{} ({})", stats.call_errors, stats.responses, pct(rate))
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

fn list_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_owned()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests;
