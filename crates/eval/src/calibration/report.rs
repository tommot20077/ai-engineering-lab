//! 校準報告(design-part2 §4)。分兩層:`CalibrationReport::new` 算數字(純函式,測試直接比對),
//! `Display` 只負責排版(用 insta snapshot 測)。同 `compare` 的做法。

use std::collections::HashMap;
use std::fmt;

use super::calibrate::{Calibration, JudgeLine};
use super::metrics::{self, Proportion, Tally};
use super::{CalibrationSet, Expected, Item, Kind, Label};
use crate::run::sum_usd;
use crate::score::VerdictResult;

/// 報告上的一個評審:別名與設定裡的模型 id。
#[derive(Debug, Clone, Copy)]
pub struct JudgeName<'a> {
    pub alias: &'a str,
    pub model: &'a str,
}

/// 算好的報告。
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationReport {
    pub dataset: String,
    pub answers: usize,
    pub expected_fail: usize,
    pub expected_pass: usize,
    /// 計畫的評審呼叫數與實際做了的(預算用完時比較少)。
    pub planned: usize,
    pub made: usize,
    pub judges: Vec<JudgeReport>,
    /// 評審兩兩之間的一致率,依命令列的順序。
    pub agreement: Vec<Agreement>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JudgeReport {
    pub alias: String,
    pub model: String,
    pub overall: Tally,
    /// 只算 `injection` 的回答:`passed_through` 就是被騙的次數。
    pub injection: Tally,
    /// 依改法分組(不含對照組),順序固定。
    pub by_kind: Vec<(Kind, Tally)>,
    /// 原本是評審自己的模型寫的回答 / 別的模型寫的。
    pub own: Tally,
    pub others: Tally,
    /// 每一次評審呼叫都算得出費用才有總額;有一次算不出來(沒有價格)就是 `None`。
    pub cost_usd: Option<f64>,
    /// 該不通過卻判通過 / 該通過卻判不通過 / 評審出錯:人最需要去讀的就是這些。
    pub missed: Vec<Finding>,
    pub false_fails: Vec<Finding>,
    pub errors: Vec<Finding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub item: String,
    pub criterion: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Agreement {
    pub a: String,
    pub b: String,
    pub rate: Option<Proportion>,
}

/// 標成 fail 的標準少於這個數,抓錯率的區間會很寬:要分辨 90% 和 80% 的評審,大約要一百條(design-part2 §4)。
const FEW_FAILS: usize = 100;

/// 依改法分組時的順序(不含對照組)。
const KINDS: [Kind; 5] = [
    Kind::Obvious,
    Kind::Subtle,
    Kind::Fabricated,
    Kind::OffTopic,
    Kind::Injection,
];

impl CalibrationReport {
    pub fn new(set: &CalibrationSet, judges: &[JudgeName<'_>], calibration: &Calibration) -> Self {
        let items: HashMap<&str, &Item> = set.items.iter().map(|i| (i.id.as_str(), i)).collect();
        let labels: Vec<&Label> = set.items.iter().flat_map(|i| &i.labels).collect();
        let expected_fail = labels
            .iter()
            .filter(|l| l.expected == Expected::Fail)
            .count();
        let mut agreement = Vec::new();
        // 兩兩一對,依命令列的順序:(1, 2)、(1, 3)、(2, 3)。
        for (index, a) in judges.iter().enumerate() {
            for b in judges.iter().skip(index + 1) {
                agreement.push(Agreement {
                    a: a.alias.to_owned(),
                    b: b.alias.to_owned(),
                    rate: metrics::agreement(paired(calibration, a.alias, b.alias)),
                });
            }
        }
        Self {
            dataset: set.dataset.clone(),
            answers: set.items.len(),
            expected_fail,
            expected_pass: labels.len() - expected_fail,
            planned: calibration.planned,
            made: calibration.lines.len(),
            judges: judges
                .iter()
                .map(|judge| JudgeReport::new(judge, &items, calibration))
                .collect(),
            agreement,
        }
    }
}

impl JudgeReport {
    fn new(judge: &JudgeName<'_>, items: &HashMap<&str, &Item>, calibration: &Calibration) -> Self {
        // 這個評審的每一行,配上它評的那筆回答(標籤在回答上)。
        let lines: Vec<(&Item, &JudgeLine)> = calibration
            .lines
            .iter()
            .filter(|line| line.judge == judge.alias)
            .filter_map(|line| Some((*items.get(line.item.as_str())?, line)))
            .collect();
        // 先篩選回答,再把(標籤, 判決)配對收集成 `Tally`(第 3 步的 `FromIterator`)。
        let tally = |keep: &dyn Fn(&Item) -> bool| -> Tally {
            lines
                .iter()
                .filter(|(item, _)| keep(item))
                .flat_map(|(item, line)| {
                    item.labels
                        .iter()
                        .zip(&line.verdicts)
                        .map(|(label, verdict)| (label.expected, verdict.result))
                })
                .collect()
        };

        let (mut missed, mut false_fails, mut errors) = (Vec::new(), Vec::new(), Vec::new());
        for (item, line) in &lines {
            for (label, verdict) in item.labels.iter().zip(&line.verdicts) {
                let list = match (label.expected, verdict.result) {
                    (Expected::Fail, VerdictResult::Pass) => &mut missed,
                    (Expected::Pass, VerdictResult::Fail) => &mut false_fails,
                    (_, VerdictResult::Error | VerdictResult::Skipped) => &mut errors,
                    _ => continue,
                };
                list.push(Finding {
                    item: item.id.clone(),
                    criterion: label.criterion.clone(),
                    reason: verdict.reason.clone(),
                });
            }
        }

        // 評審回了(有模型 id)卻沒有費用 = 沒有價格:總額未知。沒回的呼叫(逾時)沒有用量,算 0。
        let unpriced = lines
            .iter()
            .any(|(_, line)| line.judge_model.is_some() && line.cost_usd.is_none());
        let cost_usd =
            (!unpriced).then(|| sum_usd(lines.iter().filter_map(|(_, line)| line.cost_usd)));

        Self {
            alias: judge.alias.to_owned(),
            model: judge.model.to_owned(),
            overall: tally(&|_| true),
            injection: tally(&|item| item.kind == Kind::Injection),
            by_kind: KINDS
                .iter()
                .map(|&kind| (kind, tally(&|item| item.kind == kind)))
                .collect(),
            own: tally(&|item| item.source_model == judge.model),
            others: tally(&|item| item.source_model != judge.model),
            cost_usd,
            missed,
            false_fails,
            errors,
        }
    }
}

/// 兩個評審對同一筆回答、同一條標準的判決,配成一對。只有一個評審評過的回答略過。
fn paired<'a>(
    calibration: &'a Calibration,
    a: &'a str,
    b: &'a str,
) -> impl Iterator<Item = (VerdictResult, VerdictResult)> + 'a {
    calibration
        .lines
        .iter()
        .filter(move |line| line.judge == a)
        .filter_map(move |left| {
            let right = calibration
                .lines
                .iter()
                .find(|line| line.judge == b && line.item == left.item)?;
            Some(left.verdicts.iter().zip(&right.verdicts))
        })
        .flatten()
        .map(|(x, y)| (x.result, y.result))
}

/// 從一個評審的結果取出某一份清單(漏掉 / 冤枉 / 出錯)。`fn` 是函式指標:不捕獲任何東西的閉包可以直接轉成它。
/// Java 對照:`Function<JudgeReport, List<Finding>>`,傳 `JudgeReport::missed` 這種方法參考。
type PickFindings = fn(&JudgeReport) -> &[Finding];

impl fmt::Display for CalibrationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Calibrate {} ({} answers, {} criteria: {} expected fail, {} expected pass)",
            self.dataset,
            self.answers,
            self.expected_fail + self.expected_pass,
            self.expected_fail,
            self.expected_pass
        )?;
        if self.made < self.planned {
            writeln!(
                f,
                "INCOMPLETE: the budget ran out after {} of {} judge calls; the numbers cover only the calls made",
                self.made, self.planned
            )?;
        }
        for judge in &self.judges {
            writeln!(f, "  judge {} = {}", judge.alias, judge.model)?;
        }
        if self.expected_fail < FEW_FAILS {
            writeln!(
                f,
                "\nwarning: only {} criteria are expected to fail, so the catch-rate intervals are wide \
                 (telling a 90% judge from an 80% one takes about {FEW_FAILS})",
                self.expected_fail
            )?;
        }

        // `w`:名字那一欄的寬度,`{:<w$}` 靠左補空白。
        let w = self
            .judges
            .iter()
            .map(|j| j.alias.len())
            .max()
            .unwrap_or(0)
            .max(5);
        writeln!(
            f,
            "\n{:<w$}  {:<30}  {:<28}  {:>6}  fooled by injection",
            "", "catch rate", "false fail", "errors"
        )?;
        for j in &self.judges {
            writeln!(
                f,
                "{:<w$}  {:<30}  {:<28}  {:>6}  {}",
                j.alias,
                rate(j.overall.catch_rate()),
                rate(j.overall.false_fail_rate()),
                j.overall.errors,
                fraction(j.injection.passed_through, j.injection.expected_fail)
            )?;
        }
        writeln!(
            f,
            "(per criterion; Wilson 95% intervals; a judge error on a criterion that should fail is not a catch)"
        )?;

        writeln!(f, "\ncatch rate by kind:")?;
        // 先組好一整行再去掉行尾的空白:最後一欄不需要補齊。
        let header: String = KINDS
            .iter()
            .map(|&k| format!("  {:<10}", kind_name(k)))
            .collect();
        writeln!(f, "{}", format!("{:<w$}{header}", "").trim_end())?;
        for j in &self.judges {
            let cells: String = j
                .by_kind
                .iter()
                .map(|(_, t)| format!("  {:<10}", fraction(t.caught, t.expected_fail)))
                .collect();
            writeln!(f, "{}", format!("{:<w$}{cells}", j.alias).trim_end())?;
        }

        writeln!(
            f,
            "\nself-preference (catch rate on answers written by the judge's own model, vs by other models):"
        )?;
        for j in &self.judges {
            writeln!(
                f,
                "{:<w$}  own {:<16}  others {}",
                j.alias,
                share(&j.own),
                share(&j.others)
            )?;
        }

        if !self.agreement.is_empty() {
            writeln!(f, "\nagreement (criteria both judges decided):")?;
            for a in &self.agreement {
                writeln!(f, "  {} ~ {}  {}", a.a, a.b, rate(a.rate))?;
            }
        }

        let sections: [(&str, PickFindings); 3] = [
            ("missed (should fail, judged pass)", |j| &j.missed),
            ("false fails (should pass, judged fail)", |j| &j.false_fails),
            ("judge errors", |j| &j.errors),
        ];
        for (title, pick) in sections {
            let found: Vec<(&str, &Finding)> = self
                .judges
                .iter()
                .flat_map(|j| pick(j).iter().map(move |x| (j.alias.as_str(), x)))
                .collect();
            if found.is_empty() {
                writeln!(f, "\n{title}: none")?;
                continue;
            }
            writeln!(f, "\n{title}:")?;
            for (alias, x) in found {
                // 理由是評審寫的(不可信輸入):換行換成空白,一筆一行,不讓它排出假的報告內容。
                let reason = x
                    .reason
                    .as_deref()
                    .unwrap_or("-")
                    .replace(['\n', '\r'], " ");
                writeln!(f, "  {alias:<w$}  {}  {}  ({reason})", x.item, x.criterion)?;
            }
        }

        write!(f, "\ncost:")?;
        for (index, j) in self.judges.iter().enumerate() {
            let sep = if index == 0 { " " } else { ", " };
            let usd = j
                .cost_usd
                .map_or_else(|| "unknown".to_owned(), |usd| format!("${usd:.4}"));
            write!(f, "{sep}{} {usd}", j.alias)?;
        }
        writeln!(f)
    }
}

/// `96.4% [82.3, 99.4]  27/28`;沒有樣本時是 `-`。
fn rate(p: Option<Proportion>) -> String {
    p.map_or_else(
        || "-".to_owned(),
        |p| {
            format!(
                "{:.1}% [{:.1}, {:.1}]  {}/{}",
                p.rate * 100.0,
                p.low * 100.0,
                p.high * 100.0,
                p.k,
                p.n
            )
        },
    )
}

/// `3/4`;分母是 0 時是 `-`。
fn fraction(k: usize, n: usize) -> String {
    if n == 0 {
        "-".to_owned()
    } else {
        format!("{k}/{n}")
    }
}

/// 抓錯率的簡寫:`2/3 (66.7%)`。
fn share(t: &Tally) -> String {
    match t.catch_rate() {
        Some(p) => format!("{}/{} ({:.1}%)", p.k, p.n, p.rate * 100.0),
        None => "-".to_owned(),
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Control => "control",
        Kind::Obvious => "obvious",
        Kind::Subtle => "subtle",
        Kind::Fabricated => "fabricated",
        Kind::OffTopic => "off_topic",
        Kind::Injection => "injection",
    }
}

#[cfg(test)]
mod tests;
