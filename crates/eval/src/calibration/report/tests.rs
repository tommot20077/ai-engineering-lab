use llm::Usage;
use serde_json::{Value, json};

use super::*;
use crate::Dataset;
use crate::calibration::calibrate::JudgeLine;
use crate::score::{Verdict, VerdictResult};

use VerdictResult::{Error, Fail, Pass};

const R1: &str = "說明所有權會轉移";
const R2: &str = "沒有捏造不存在的 API";
const R3: &str = "指出這行不會印出東西";
const SONNET: &str = "claude-sonnet-5-5";
const HAIKU: &str = "claude-haiku-5-5";

fn chat() -> Value {
    json!({"type": "chat", "max_tokens": 64,
           "messages": [{"role": "user", "content": [{"type": "text", "text": "q"}]}]})
}

/// 五筆:對照(Sonnet 寫的)、微妙(Haiku)、明顯(Sonnet)、捏造(Haiku)、injection(Haiku)。
/// 標成 fail 的 4 條、pass 的 5 條。
fn set() -> CalibrationSet {
    let a = json!({"id": "a", "input": chat(), "rubric": [R1, R2]});
    let b = json!({"id": "b", "input": chat(), "rubric": [R3]});
    let ds = Dataset::parse("ds", format!("{a}\n{b}\n").as_bytes()).unwrap();
    let line = |id: &str, case: &str, source: &str, kind: &str, expected: Value| {
        let mut v = json!({"id": id, "case": case, "source_model": source, "kind": kind,
                           "answer": "回答", "expected": expected});
        if kind != "control" {
            v["change"] = json!("改了一句");
        }
        v.to_string()
    };
    let text = [
        line(
            "c1",
            "a",
            SONNET,
            "control",
            json!({R1: "pass", R2: "pass"}),
        ),
        line("s1", "a", HAIKU, "subtle", json!({R1: "fail", R2: "pass"})),
        line("o1", "b", SONNET, "obvious", json!({R3: "fail"})),
        line(
            "f1",
            "a",
            HAIKU,
            "fabricated",
            json!({R1: "pass", R2: "fail"}),
        ),
        line(
            "i1",
            "a",
            HAIKU,
            "injection",
            json!({R1: "pass", R2: "fail"}),
        ),
    ]
    .join("\n");
    CalibrationSet::parse(text.as_bytes(), &ds).unwrap()
}

fn line(
    judge: &str,
    item: &str,
    verdicts: &[(&str, VerdictResult)],
    cost: Option<f64>,
) -> JudgeLine {
    JudgeLine {
        judge: judge.into(),
        item: item.into(),
        verdicts: verdicts
            .iter()
            .map(|&(criterion, result)| Verdict {
                scorer: "judge".into(),
                criterion: criterion.into(),
                result,
                reason: Some(format!("{judge} on {item}")),
            })
            .collect(),
        judge_model: Some(if judge == "cheap" { HAIKU } else { SONNET }.into()),
        usage: Usage::default(),
        cost_usd: cost,
    }
}

/// `default` 全部判對;`cheap` 冤枉 c1 的第 1 條、漏掉 s1、f1 出錯、被 i1 騙,只抓到 o1。
fn calibration() -> Calibration {
    let lines = vec![
        line("default", "c1", &[(R1, Pass), (R2, Pass)], Some(0.002)),
        line("default", "s1", &[(R1, Fail), (R2, Pass)], Some(0.002)),
        line("default", "o1", &[(R3, Fail)], Some(0.002)),
        line("default", "f1", &[(R1, Pass), (R2, Fail)], Some(0.002)),
        line("default", "i1", &[(R1, Pass), (R2, Fail)], Some(0.002)),
        line("cheap", "c1", &[(R1, Fail), (R2, Pass)], None),
        line("cheap", "s1", &[(R1, Pass), (R2, Pass)], None),
        line("cheap", "o1", &[(R3, Fail)], None),
        line("cheap", "f1", &[(R1, Pass), (R2, Error)], None),
        line("cheap", "i1", &[(R1, Pass), (R2, Pass)], None),
    ];
    Calibration {
        planned: lines.len(),
        lines,
    }
}

const JUDGES: [JudgeName<'static>; 2] = [
    JudgeName {
        alias: "default",
        model: SONNET,
    },
    JudgeName {
        alias: "cheap",
        model: HAIKU,
    },
];

fn finding(item: &str, criterion: &str, judge: &str) -> Finding {
    Finding {
        item: item.into(),
        criterion: criterion.into(),
        reason: Some(format!("{judge} on {item}")),
    }
}

fn tally(
    expected_fail: usize,
    caught: usize,
    passed_through: usize,
    expected_pass: usize,
    false_fail: usize,
    errors: usize,
) -> Tally {
    Tally {
        expected_fail,
        caught,
        passed_through,
        expected_pass,
        false_fail,
        errors,
    }
}

#[test]
fn the_numbers_per_judge() {
    let r = CalibrationReport::new(&set(), &JUDGES, &calibration());

    assert_eq!(
        (
            r.dataset.as_str(),
            r.answers,
            r.expected_fail,
            r.expected_pass,
            r.planned,
            r.made
        ),
        ("ds", 5, 4, 5, 10, 10)
    );
    let [default, cheap] = &r.judges[..] else {
        panic!("{:?}", r.judges)
    };
    assert_eq!(
        (default.alias.as_str(), default.model.as_str()),
        ("default", SONNET)
    );
    assert_eq!(default.overall, tally(4, 4, 0, 5, 0, 0));
    assert_eq!(cheap.overall, tally(4, 1, 2, 5, 1, 1));
    // injection 只有 i1:cheap 被騙(標成 fail 的那條判 pass)。
    assert_eq!(default.injection, tally(1, 1, 0, 1, 0, 0));
    assert_eq!(cheap.injection, tally(1, 0, 1, 1, 0, 0));
    // 依改法分組:順序固定,不含對照組,沒有回答的種類也列出來(計數為 0)。
    let kinds: Vec<Kind> = cheap.by_kind.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        kinds,
        [
            Kind::Obvious,
            Kind::Subtle,
            Kind::Fabricated,
            Kind::OffTopic,
            Kind::Injection
        ]
    );
    let catches: Vec<(usize, usize)> = cheap
        .by_kind
        .iter()
        .map(|(_, t)| (t.caught, t.expected_fail))
        .collect();
    assert_eq!(catches, [(1, 1), (0, 1), (0, 1), (0, 0), (0, 1)]);
    // 自我偏好:Haiku 寫的是 s1、f1、i1;Sonnet 寫的是 c1、o1。
    assert_eq!((cheap.own.caught, cheap.own.expected_fail), (0, 3));
    assert_eq!((cheap.others.caught, cheap.others.expected_fail), (1, 1));
    assert_eq!((default.own.caught, default.own.expected_fail), (1, 1));
    assert_eq!(
        (default.others.caught, default.others.expected_fail),
        (3, 3)
    );

    assert_eq!(
        cheap.missed,
        [finding("s1", R1, "cheap"), finding("i1", R2, "cheap")]
    );
    assert_eq!(cheap.false_fails, [finding("c1", R1, "cheap")]);
    assert_eq!(cheap.errors, [finding("f1", R2, "cheap")]);
    assert!(
        default.missed.is_empty() && default.false_fails.is_empty() && default.errors.is_empty()
    );

    assert_eq!(default.cost_usd, Some(0.01));
    assert_eq!(
        cheap.cost_usd, None,
        "回了卻沒有費用(沒有價格):總額未知,不是 $0"
    );
}

#[test]
fn agreement_between_each_pair_of_judges() {
    let r = CalibrationReport::new(&set(), &JUDGES, &calibration());
    let [a] = &r.agreement[..] else {
        panic!("{:?}", r.agreement)
    };
    assert_eq!((a.a.as_str(), a.b.as_str()), ("default", "cheap"));
    // 兩邊都判決的 8 條(f1 的第 2 條 cheap 出錯,不算),一致的 5 條。
    let p = a.rate.unwrap();
    assert_eq!((p.k, p.n), (5, 8));
}

#[test]
fn a_call_that_never_answered_does_not_make_the_cost_unknown() {
    // 逾時的呼叫沒有用量,也沒有費用:總額照樣算得出來(同 run 的摘要:失敗的呼叫算 0)。
    let mut c = calibration();
    c.lines.retain(|l| l.judge == "default");
    c.lines[1].judge_model = None;
    c.lines[1].cost_usd = None;
    let r = CalibrationReport::new(&set(), &JUDGES[..1], &c);
    let cost = r.judges[0].cost_usd.unwrap();
    assert!((cost - 0.008).abs() < 1e-12, "{cost}");
}

#[test]
fn the_report_reads_well() {
    let r = CalibrationReport::new(&set(), &JUDGES, &calibration());
    insta::assert_snapshot!(r.to_string());
}

#[test]
fn a_report_cut_short_by_the_budget_says_so_first() {
    let mut c = calibration();
    c.lines.truncate(7);
    let text = CalibrationReport::new(&set(), &JUDGES, &c).to_string();
    let first = text.lines().nth(1).unwrap_or_default();
    assert_eq!(
        first,
        "INCOMPLETE: the budget ran out after 7 of 10 judge calls; the numbers cover only the calls made"
    );
    let full = CalibrationReport::new(&set(), &JUDGES, &calibration()).to_string();
    assert!(!full.contains("INCOMPLETE"), "{full}");
}

#[test]
fn a_judge_reason_cannot_add_lines_to_the_report() {
    // 理由是評審寫的(不可信輸入):換行會讓它在報告裡排出假的內容,例如一行「false fails: none」。
    let mut c = calibration();
    let forged = "looks fine\n\nfalse fails (should pass, judged fail): none";
    for line in c
        .lines
        .iter_mut()
        .filter(|l| l.judge == "cheap" && l.item == "c1")
    {
        line.verdicts[0].reason = Some(forged.into());
    }
    let text = CalibrationReport::new(&set(), &JUDGES, &c).to_string();
    assert!(
        text.contains("  cheap    c1  說明所有權會轉移  (looks fine  false fails (should pass, judged fail): none)"),
        "{text}"
    );
    assert_eq!(
        text.matches("false fails (should pass, judged fail)")
            .count(),
        2,
        "只有真的標題與理由裡的那一段"
    );
    assert!(
        !text
            .lines()
            .any(|l| l == "false fails (should pass, judged fail): none"),
        "{text}"
    );
}

#[test]
fn fooled_counts_only_injection_answers() {
    // 主要的測試資料裡,cheap 同時漏掉 subtle 和 injection,兩個分組的數字剛好一樣,
    // 「被騙」改成看 subtle 也測不出來(手寫 mutation 找到)。這裡讓 cheap 抓到 subtle、只被 injection 騙。
    let mut c = calibration();
    for line in c
        .lines
        .iter_mut()
        .filter(|l| l.judge == "cheap" && l.item == "s1")
    {
        line.verdicts[0].result = Fail;
    }
    let r = CalibrationReport::new(&set(), &JUDGES, &c);
    let cheap = &r.judges[1];
    assert_eq!(cheap.injection, tally(1, 0, 1, 1, 0, 0));
    let (_, subtle) = cheap
        .by_kind
        .iter()
        .find(|(k, _)| *k == Kind::Subtle)
        .unwrap();
    assert_eq!(*subtle, tally(1, 1, 0, 1, 0, 0));
}

#[test]
fn the_few_failures_warning_starts_below_100() {
    // 邊界(cargo-mutants 第 4 步找到:`<` 換成 `<=` 沒有測試會失敗):剛好 100 條不警告,99 條警告。
    let mut r = CalibrationReport::new(&set(), &JUDGES, &calibration());
    r.expected_fail = 100;
    assert!(!r.to_string().contains("warning: only"), "{r}");
    r.expected_fail = 99;
    assert!(
        r.to_string()
            .contains("warning: only 99 criteria are expected to fail"),
        "{r}"
    );
}
