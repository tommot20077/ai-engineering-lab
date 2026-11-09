//! 統計:從打分算出兩種指標,並用 paired bootstrap 算兩次執行差異的信賴區間(design §7、D-032、D-035)。
//!
//! 全部是純函式:輸入打分的行,輸出數字;亂數也是自己寫的、固定種子,同樣的輸入一定得到同樣的數字。
//!
//! ```text
//! 一筆回應 ──→ 全部通過(是 / 否 / 不知道)、部分得分(通過幾成)
//! 同一題的多次抽樣 ──平均──→ 案例分數
//! 全部案例 ──平均──→ 這次執行的分數
//! 兩次執行依案例配對 ──bootstrap──→ 「B − A」的 95% 信賴區間
//! ```

use std::collections::{BTreeMap, BTreeSet};

use crate::score::{ScoreLine, ScoreResult, VerdictResult};

/// bootstrap 重抽的次數。
pub const RESAMPLES: usize = 10_000;
/// 固定的亂數種子,寫進報告(design §7)。
pub const SEED: u64 = 0x5EED;
/// 案例少於這個數字時要警告:區間會很寬。
pub const FEW_CASES: usize = 30;

/// 一筆回應的兩個分數。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResponseScore {
    /// 全部的檢查與標準都通過。`None` = 不知道(有打分器出錯,而且沒有不通過的):不列入分母。
    pub all_pass: Option<bool>,
    /// 通過的標準佔幾成。`Error` 不列入分母,`Skipped` 算 0 分(design §7)。全部都是 `Error` 時是 `None`。
    pub partial: Option<f64>,
}

/// 一個案例的分數:各次抽樣的平均(design §5)。
#[derive(Debug, Clone, PartialEq)]
pub struct CaseScore {
    pub case_id: String,
    pub samples: usize,
    pub all_pass: Option<f64>,
    pub partial: Option<f64>,
}

/// 一個指標在整次執行上的平均。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metric {
    /// 案例分數的平均;沒有任何案例算得出來時是 `None`。
    pub mean: Option<f64>,
    /// 算得出這個指標的案例數(分母)。
    pub cases: usize,
}

/// 一個案例的一條標準,在各次抽樣上的結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CriterionRate {
    pub case_id: String,
    pub scorer: String,
    pub criterion: String,
    pub pass: usize,
    /// 分母:通過 + 不通過 + 沒評(`Skipped` 算 0 分)。
    pub counted: usize,
    /// 打分器出錯的次數(不在分母裡)。
    pub errors: usize,
}

impl CriterionRate {
    pub fn rate(&self) -> Option<f64> {
        ratio(self.pass, self.counted)
    }
}

/// 一次執行(一份打分)的統計。
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub cases: usize,
    pub responses: usize,
    pub all_pass: Metric,
    pub partial: Metric,
    /// 「不知道算不算全部通過」的回應數。大於 0 時報告要提醒「打分不完整」。
    pub incomplete: usize,
    /// 呼叫模型就失敗的回應數(已經算成沒通過,錯誤率另外列,design §5)。
    pub call_errors: usize,
    /// 依案例、打分器、標準排序。
    pub criteria: Vec<CriterionRate>,
}

/// 要比較的指標。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    /// 全部通過率:做決定用(design §7)。
    AllPass,
    /// 部分得分:看進度、做診斷用。
    Partial,
}

impl Measure {
    fn of(self, case: &CaseScore) -> Option<f64> {
        match self {
            Measure::AllPass => case.all_pass,
            Measure::Partial => case.partial,
        }
    }
}

/// 兩次執行依 `case_id` 配對的結果。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pairing {
    /// (案例 id, A 的分數, B 的分數),依 id 排序。
    pub pairs: Vec<(String, f64, f64)>,
    /// 只出現在一邊的案例:列出來,不納入比較(design §7)。
    pub only_a: Vec<String>,
    pub only_b: Vec<String>,
    /// 兩邊都有,但至少一邊算不出這個指標(全部是 `Error`)。
    pub unscored: Vec<String>,
}

impl Pairing {
    /// 只留分數,給 `paired_bootstrap`。
    pub fn values(&self) -> Vec<(f64, f64)> {
        self.pairs.iter().map(|(_, a, b)| (*a, *b)).collect()
    }
}

/// 「B − A」的信賴區間。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interval {
    pub cases: usize,
    /// 實際觀察到的差:B 的平均 − A 的平均。
    pub diff: f64,
    /// 95% 信賴區間的下界與上界。
    pub low: f64,
    pub high: f64,
    pub resamples: usize,
    pub seed: u64,
}

impl Interval {
    /// 區間不含 0,才說「有差異」;含 0 是「看不出差異」,不是「一樣好」(design §7)。
    pub fn excludes_zero(&self) -> bool {
        self.low > 0.0 || self.high < 0.0
    }

    /// 不比 A 差超過 `delta`(non-inferiority):下界高於 −δ。階段 2 切流量用;δ 在 011 決定。
    pub fn non_inferior(&self, delta: f64) -> bool {
        self.low > -delta
    }

    /// 案例太少,區間會很寬:報告要警告。
    pub fn few_cases(&self) -> bool {
        self.cases < FEW_CASES
    }
}

/// 一筆回應的分數。
pub fn response_score(line: &ScoreLine) -> ResponseScore {
    let all_pass = match line.result {
        ScoreResult::Pass => Some(true),
        // 呼叫失敗算沒通過(design §5:跳過失敗的題目會讓分數虛高)。
        ScoreResult::Fail | ScoreResult::CallError => Some(false),
        ScoreResult::Incomplete => None,
    };
    let (mut pass, mut counted) = (0, 0);
    for verdict in &line.verdicts {
        match verdict.result {
            VerdictResult::Pass => {
                pass += 1;
                counted += 1;
            }
            // 沒評的算 0 分:被跳過的剛好都是差的回答(design §7)。
            VerdictResult::Fail | VerdictResult::Skipped => counted += 1,
            // 打分器出錯跟回答好壞無關:不列入分母。
            VerdictResult::Error => {}
        }
    }
    ResponseScore {
        all_pass,
        partial: ratio(pass, counted),
    }
}

/// 依案例分組,各次抽樣取平均。依 `case_id` 排序。
/// 傳進來的應該是「目前有效」的打分(每個 (案例, 抽樣) 一行,評的是最新的回應)。
pub fn case_scores(lines: &[ScoreLine]) -> Vec<CaseScore> {
    // 用 `BTreeMap` 分組而不是 `HashMap`:key 有排序,輸出的順序固定(報告、bootstrap 的結果才可重現)。
    // `entry(..).or_default()`:沒有這個 key 就先放一個空的 Vec,再回傳它的可變參考。
    // Java 對照:`map.computeIfAbsent(id, k -> new ArrayList<>()).add(line)`。
    let mut groups: BTreeMap<&str, Vec<ResponseScore>> = BTreeMap::new();
    for line in lines {
        groups
            .entry(line.case_id.as_str())
            .or_default()
            .push(response_score(line));
    }
    groups
        .into_iter()
        .map(|(case_id, scores)| CaseScore {
            case_id: case_id.to_owned(),
            samples: scores.len(),
            all_pass: mean(scores.iter().filter_map(|s| s.all_pass.map(f64::from))),
            partial: mean(scores.iter().filter_map(|s| s.partial)),
        })
        .collect()
}

/// 一份打分的統計。
pub fn stats(lines: &[ScoreLine]) -> Stats {
    let cases = case_scores(lines);
    let metric = |pick: fn(&CaseScore) -> Option<f64>| {
        let values: Vec<f64> = cases.iter().filter_map(pick).collect();
        Metric {
            mean: mean(values.iter().copied()),
            cases: values.len(),
        }
    };

    // (案例, 打分器, 標準) → 計數。三個欄位組成的 tuple 可以直接當 key,依序比較(先案例、再打分器、再標準)。
    let mut criteria: BTreeMap<(&str, &str, &str), CriterionRate> = BTreeMap::new();
    for line in lines {
        for v in &line.verdicts {
            let key = (
                line.case_id.as_str(),
                v.scorer.as_str(),
                v.criterion.as_str(),
            );
            let rate = criteria.entry(key).or_insert_with(|| CriterionRate {
                case_id: line.case_id.clone(),
                scorer: v.scorer.clone(),
                criterion: v.criterion.clone(),
                pass: 0,
                counted: 0,
                errors: 0,
            });
            match v.result {
                VerdictResult::Pass => {
                    rate.pass += 1;
                    rate.counted += 1;
                }
                VerdictResult::Fail | VerdictResult::Skipped => rate.counted += 1,
                VerdictResult::Error => rate.errors += 1,
            }
        }
    }

    Stats {
        cases: cases.len(),
        responses: lines.len(),
        all_pass: metric(|c| c.all_pass),
        partial: metric(|c| c.partial),
        incomplete: lines
            .iter()
            .filter(|l| l.result == ScoreResult::Incomplete)
            .count(),
        call_errors: lines
            .iter()
            .filter(|l| l.result == ScoreResult::CallError)
            .count(),
        criteria: criteria.into_values().collect(),
    }
}

/// 依 `case_id` 配對兩次執行的案例分數。
pub fn pair(a: &[CaseScore], b: &[CaseScore], measure: Measure) -> Pairing {
    let by_id = |cases: &[CaseScore]| -> BTreeMap<String, Option<f64>> {
        cases
            .iter()
            .map(|c| (c.case_id.clone(), measure.of(c)))
            .collect()
    };
    let (a, b) = (by_id(a), by_id(b));
    let mut pairing = Pairing::default();
    // 兩邊的 id 合起來(`BTreeSet`:去重、排序),逐一分類。
    let ids: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    for id in ids {
        match (a.get(id), b.get(id)) {
            (Some(Some(x)), Some(Some(y))) => pairing.pairs.push((id.clone(), *x, *y)),
            (Some(_), Some(_)) => pairing.unscored.push(id.clone()),
            (Some(_), None) => pairing.only_a.push(id.clone()),
            (None, _) => pairing.only_b.push(id.clone()),
        }
    }
    pairing
}

/// paired bootstrap:從配對的案例中有放回地抽 `resamples` 次,每次算「B − A」的平均差,
/// 取第 2.5 與第 97.5 百分位當 95% 信賴區間。沒有案例或不重抽時是 `None`。
///
/// 為什麼要「配對」:同一題對兩個模型都難或都簡單,題目本身的難易是最大的雜訊。
/// 每題先算 B − A,再對差值重抽,題目難易就互相抵銷,區間比兩邊各自重抽窄很多。
pub fn paired_bootstrap(pairs: &[(f64, f64)], resamples: usize, seed: u64) -> Option<Interval> {
    if pairs.is_empty() || resamples == 0 {
        return None;
    }
    let diffs: Vec<f64> = pairs.iter().map(|(a, b)| b - a).collect();
    let n = diffs.len();
    let mut rng = SplitMix64(seed);
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| {
            let total: f64 = (0..n).filter_map(|_| diffs.get(rng.below(n))).sum();
            total / n as f64
        })
        .collect();
    // `total_cmp`:f64 沒有 `Ord`(NaN 跟誰都不能比),`sort()` 不能直接用;`total_cmp` 定義了一個完整的順序。
    // Java 對照:`Arrays.sort(double[])` 內部用的就是 `Double.compare`,也是完整順序。
    means.sort_by(f64::total_cmp);
    // 第 2.5 與第 97.5 百分位:兩邊各切掉 2.5%(10,000 次時是第 250 與第 9,749 個,從 0 數)。
    let tail = resamples * 25 / 1000;
    let low = *means.get(tail)?;
    let high = *means.get(resamples - 1 - tail)?;
    Some(Interval {
        cases: n,
        diff: diffs.iter().sum::<f64>() / n as f64,
        low,
        high,
        resamples,
        seed,
    })
}

/// 平均;沒有任何值時是 `None`(「沒有資料」不能當成 0)。
fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, count) = values.fold((0.0, 0usize), |(sum, count), v| (sum + v, count + 1));
    (count > 0).then(|| sum / count as f64)
}

fn ratio(part: usize, whole: usize) -> Option<f64> {
    (whole > 0).then(|| part as f64 / whole as f64)
}

/// SplitMix64:很小、很快、品質夠用的亂數產生器(Java 的 `SplittableRandom` 用的就是它)。
///
/// 不用 `fastrand`:它不保證不同版本產生同樣的數列。這十幾行自己寫,同樣的種子永遠得到同樣的數字,
/// 存起來的報告才重現得出來。不是密碼學等級:這裡只需要「看起來隨機、可重現」。
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        // 每次加一個固定的奇數(黃金比例 × 2^64),再把位元打散。`wrapping_*`:溢位時繞回去,這正是演算法要的;
        // 一般的 `+`、`*` 在 debug 模式溢位會 panic。Java 的 long 運算本來就是繞回去的,所以 Java 版不需要特別寫。
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `0..n` 之間的整數(Lemire 的乘法再右移,不用 `%`:取餘數在 n 不整除 2^64 時會偏向小的數)。
    fn below(&mut self, n: usize) -> usize {
        // 64 位元 × n 放進 128 位元,取高的 64 位元:結果一定小於 n。
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }
}

#[cfg(test)]
mod tests;
