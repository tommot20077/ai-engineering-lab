//! 打分流程的測試:真的 `Judge` + 假的評審 provider,執行目錄直接用 `store` 寫出來(不必先跑 `run`)。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use llm::{
    Content, EventStream, LlmProvider, ProviderMetadata, RecordedError, Request, Response,
    StopKind, StopReason,
};

use super::*;
use crate::store::DatasetInfo;

/// 假的評審。看 prompt 決定怎麼回:回答裡有 `BAD` → 每條都不通過;有 `HANG` → 永遠不回;其他 → 每條都通過。
/// 每次用掉 1000 個輸入、100 個輸出、300 個讀快取、50 個寫快取 token。
struct FakeJudge {
    calls: AtomicUsize,
    served: &'static str,
}

impl FakeJudge {
    fn new(served: &'static str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            served,
        })
    }

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

impl LlmProvider for FakeJudge {
    fn stream(&self, _request: Request) -> EventStream {
        Box::pin(futures::stream::empty())
    }

    fn complete(&self, request: Request) -> BoxFuture<'_, llm::Result<Response>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let text = prompt(&request);
            let answer = text.split("## Answer to grade").nth(1).unwrap_or_default();
            if answer.contains("HANG") {
                return futures::future::pending().await;
            }
            // 「## Criteria」那一段有幾行,就回幾條。
            let criteria = text
                .split("## Criteria\n")
                .nth(1)
                .unwrap_or_default()
                .split("\n\n")
                .next()
                .unwrap_or_default()
                .lines()
                .count();
            let passed = !answer.contains("BAD");
            let verdicts: Vec<_> = (1..=criteria)
                .map(|n| json!({ "criterion": n, "passed": passed, "reason": "fake" }))
                .collect();
            Ok(Response {
                id: None,
                model: self.served.into(),
                content: vec![Content::Text {
                    text: json!({ "verdicts": verdicts }).to_string(),
                    meta: ProviderMetadata::default(),
                }],
                stop: StopReason {
                    kind: StopKind::EndTurn,
                    raw: "end_turn".into(),
                },
                usage: Usage {
                    input_tokens: 1000,
                    output_tokens: 100,
                    cache_read_tokens: 300,
                    cache_write_tokens: 50,
                },
                meta: ProviderMetadata::default(),
            })
        })
    }
}

/// 評審每次:(1000 + 300 + 50) × $2 + 100 × $10(每百萬 token)= $0.0037。
/// 快取的 token 也按輸入價計(`run::cost`:估高不估低)。
const PRICE: Price = Price {
    input: 2.0,
    output: 10.0,
};
const PER_JUDGE_CALL: f64 = 0.0037;

/// 案例:題目 `q-<id>`、檢查「要有 x」;`rubric` 不是空的就加上評分標準。
fn case_line(id: &str, rubric: &[&str]) -> String {
    format!(
        r#"{{"id":"{id}","input":{{"type":"chat","max_tokens":64,"messages":[{{"role":"user","content":[{{"type":"text","text":"q-{id}"}}]}}]}},"checks":[{{"type":"contains","value":"x"}}],"rubric":{}}}"#,
        serde_json::to_string(rubric).unwrap()
    )
}

/// 一筆回應:`Some(回答)` 或 `None`(呼叫失敗)。
type Answer<'a> = (&'a str, u32, Option<&'a str>);

struct Setup {
    _tmp: tempfile::TempDir,
    run: PathBuf,
    dataset: PathBuf,
}

/// 寫好資料集與一次執行(被測模型 `cheap`,實際回答的是 `cheap-id`)。
fn setup(cases: &[String], answers: &[Answer<'_>]) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let dataset = tmp.path().join("ds.jsonl");
    std::fs::write(&dataset, cases.join("\n")).unwrap();
    let ds = Dataset::load(&dataset).unwrap();
    let meta = RunMeta {
        eval_version: "0.1.0".into(),
        started_at: t("2026-11-01T12:00:00Z"),
        finished_at: None,
        dataset: DatasetInfo {
            name: ds.name.clone(),
            path: dataset.clone(),
            sha256: ds.sha256.clone(),
            cases: ds.cases.len(),
        },
        config: RunConfig {
            model: "cheap".into(),
            repeats: 1,
            tags: vec![],
            limit: None,
            label: None,
        },
        git_commit: None,
        summary: None,
    };
    let dir = RunDir::create(&tmp.path().join("runs"), &meta).unwrap();
    append(&dir, &ds, answers);
    Setup {
        run: dir.path().to_path_buf(),
        _tmp: tmp,
        dataset,
    }
}

fn append(dir: &RunDir, ds: &Dataset, answers: &[Answer<'_>]) {
    let mut out = dir.appender().unwrap();
    for (id, sample, answer) in answers {
        let case = ds.cases.iter().find(|c| c.id == *id).unwrap();
        let result = match answer {
            Some(text) => CallResult::Ok {
                output: Output::Chat(Response {
                    id: None,
                    model: "cheap-id".into(),
                    content: vec![Content::Text {
                        text: (*text).into(),
                        meta: ProviderMetadata::default(),
                    }],
                    stop: StopReason {
                        kind: StopKind::EndTurn,
                        raw: "end_turn".into(),
                    },
                    usage: Usage::default(),
                    meta: ProviderMetadata::default(),
                }),
            },
            None => CallResult::Error {
                error: RecordedError {
                    kind: "http".into(),
                    status: Some(529),
                    message: "overloaded".into(),
                },
            },
        };
        out.append(&ResponseLine {
            case_id: (*id).into(),
            sample: *sample,
            input_sha256: case.input_sha256(),
            result,
            latency_ms: 1,
            cost_usd: None,
        })
        .unwrap();
    }
}

fn t(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn options() -> ExecOptions {
    ExecOptions {
        concurrency: 2,
        timeout: Duration::from_secs(300),
        budget_usd: None,
        price: Some(PRICE),
    }
}

fn judge(fake: &Arc<FakeJudge>, model: &str) -> Judge {
    Judge::new(fake.clone(), model, "low")
}

fn setup_with<'a>(name: &'a str, judge: Option<&'a Judge>) -> ScoreSetup<'a> {
    ScoreSetup {
        name,
        judge,
        skip_on_check_failure: true,
    }
}

fn lines(run: &Path, name: &str) -> Vec<ScoreLine> {
    RunDir::open(run)
        .unwrap()
        .scores(name)
        .unwrap()
        .read_lines()
        .unwrap()
}

fn score_meta(run: &Path, name: &str) -> Option<ScoreMeta> {
    RunDir::open(run)
        .unwrap()
        .scores(name)
        .unwrap()
        .read_meta()
        .unwrap()
}

/// (案例, 抽樣, 結果, 評審狀態),依案例與抽樣排序(並行打分,寫入的順序不固定)。
fn outcomes(lines: &[ScoreLine]) -> Vec<(String, u32, ScoreResult, JudgeStatus)> {
    let mut out: Vec<_> = lines
        .iter()
        .map(|l| (l.case_id.clone(), l.sample, l.result, l.judge))
        .collect();
    out.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    out
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

fn verdict(result: VerdictResult) -> Verdict {
    Verdict {
        scorer: "check".into(),
        criterion: "c".into(),
        result,
        reason: None,
    }
}

#[test]
fn a_response_passes_only_when_every_verdict_passes() {
    use VerdictResult::{Error, Fail, Pass};
    let r = |vs: &[VerdictResult]| result_of(&vs.iter().map(|v| verdict(*v)).collect::<Vec<_>>());
    assert_eq!(r(&[Pass, Pass]), ScoreResult::Pass);
    assert_eq!(r(&[Pass, Fail]), ScoreResult::Fail);
    // 有不通過的,評審出錯也改變不了結果。
    assert_eq!(r(&[Error, Fail, Pass]), ScoreResult::Fail);
    assert_eq!(r(&[Pass, Error]), ScoreResult::Incomplete);
    // 沒有任何證據,不能說它通過。
    assert_eq!(r(&[]), ScoreResult::Incomplete);
}

#[tokio::test(start_paused = true)]
async fn checks_only_scoring_writes_one_line_per_response_and_a_meta() {
    let s = setup(
        &[case_line("a", &[]), case_line("b", &[])],
        &[
            ("a", 0, Some("has x")),
            ("a", 1, Some("nope")),
            ("b", 0, None),
        ],
    );

    let report = score(
        &s.run,
        &setup_with("checks", None),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();

    let got = lines(&s.run, "checks");
    assert_eq!(
        outcomes(&got),
        [
            ("a".into(), 0, ScoreResult::Pass, JudgeStatus::NotNeeded),
            ("a".into(), 1, ScoreResult::Fail, JudgeStatus::NotNeeded),
            (
                "b".into(),
                0,
                ScoreResult::CallError,
                JudgeStatus::NotNeeded
            ),
        ]
    );
    let fail = got.iter().find(|l| l.sample == 1).unwrap();
    assert_eq!(fail.response, 1);
    assert_eq!(
        fail.verdicts,
        [Verdict {
            scorer: "check".into(),
            criterion: r#"contains "x""#.into(),
            result: VerdictResult::Fail,
            reason: Some(r#""x" not found"#.into()),
        }]
    );
    // 呼叫失敗:沒有回答可以評,也就沒有判決。
    assert!(
        got.iter()
            .find(|l| l.case_id == "b")
            .unwrap()
            .verdicts
            .is_empty()
    );
    assert!(
        got.iter()
            .all(|l| l.cost_usd.is_none() && l.judge_usage.is_none())
    );

    let expected = ScoreSummary {
        responses: 3,
        pass: 1,
        fail: 1,
        incomplete: 0,
        call_errors: 1,
        missing: 0,
        judge_calls: 0,
        judge_skipped: 0,
        judge_models: vec![],
        judge_usage: Usage::default(),
        // 沒叫評審:費用是確定的 0,不是「未知」。
        cost_usd: Some(0.0),
    };
    assert_eq!(report.summary, expected);
    assert!(report.warnings.is_empty());
    let meta = score_meta(&s.run, "checks").unwrap();
    assert_eq!(meta.summary, Some(expected));
    assert_eq!(meta.config.judge, None);
    assert!(meta.config.skip_on_check_failure);
    assert_eq!(meta.finished_at, Some(t("2026-11-02T00:00:00Z")));
    assert_eq!(meta.started_at, t("2026-11-02T00:00:00Z"));
}

#[tokio::test(start_paused = true)]
async fn the_judge_grades_the_rubric_after_the_checks() {
    let s = setup(
        &[case_line("a", &["r1", "r2"]), case_line("b", &[])],
        &[
            ("a", 0, Some("x good")),
            ("a", 1, Some("x BAD")),
            ("b", 0, Some("x")),
        ],
    );
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");

    let report = score(
        &s.run,
        &setup_with("j", Some(&j)),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();

    // 沒有評分標準的 b 不叫評審。
    assert_eq!(fake.calls(), 2);
    let got = lines(&s.run, "j");
    assert_eq!(
        outcomes(&got),
        [
            ("a".into(), 0, ScoreResult::Pass, JudgeStatus::Called),
            ("a".into(), 1, ScoreResult::Fail, JudgeStatus::Called),
            ("b".into(), 0, ScoreResult::Pass, JudgeStatus::NotNeeded),
        ]
    );
    let good = got
        .iter()
        .find(|l| l.case_id == "a" && l.sample == 0)
        .unwrap();
    // 先檢查、再評審的逐條結果。
    let criteria: Vec<(&str, &str, VerdictResult)> = good
        .verdicts
        .iter()
        .map(|v| (v.scorer.as_str(), v.criterion.as_str(), v.result))
        .collect();
    assert_eq!(
        criteria,
        [
            ("check", r#"contains "x""#, VerdictResult::Pass),
            ("judge", "r1", VerdictResult::Pass),
            ("judge", "r2", VerdictResult::Pass),
        ]
    );
    assert_eq!(good.judge_model.as_deref(), Some("judge-id"));
    assert_eq!(good.judge_usage.map(|u| u.output_tokens), Some(100));
    assert!(approx(good.cost_usd.unwrap(), PER_JUDGE_CALL));

    let summary = report.summary;
    assert_eq!(
        (
            summary.pass,
            summary.fail,
            summary.judge_calls,
            summary.judge_skipped
        ),
        (2, 1, 2, 0)
    );
    assert_eq!(summary.judge_models, ["judge-id"]);
    // 四種 token 都累加:快取有沒有生效要寫進報告(design §6)。
    // (cargo-mutants:快取兩欄的 `+=` 換成 `-=`、`*=` 時沒有測試失敗,因為假評審原本回 0。)
    assert_eq!(
        summary.judge_usage,
        Usage {
            input_tokens: 2000,
            output_tokens: 200,
            cache_read_tokens: 600,
            cache_write_tokens: 100,
        }
    );
    assert!(approx(summary.cost_usd.unwrap(), 2.0 * PER_JUDGE_CALL));
    let config = score_meta(&s.run, "j").unwrap().config;
    assert_eq!(
        config.judge,
        Some(JudgeConfig {
            model: "default".into(),
            effort: "low".into()
        })
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_check_skips_the_judge_unless_told_to_judge_all() {
    let cases = [case_line("a", &["r1"])];
    // 第 0 筆沒有 x(檢查沒過)。第 1 筆:regex 壞掉的情況另外測,這裡是正常通過的。
    let answers = [("a", 0, Some("no")), ("a", 1, Some("x"))];

    let s = setup(&cases, &answers);
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");
    let report = score(
        &s.run,
        &setup_with("skip", Some(&j)),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();
    assert_eq!(fake.calls(), 1);
    assert_eq!(
        outcomes(&lines(&s.run, "skip")),
        [
            ("a".into(), 0, ScoreResult::Fail, JudgeStatus::Skipped),
            ("a".into(), 1, ScoreResult::Pass, JudgeStatus::Called),
        ]
    );
    assert_eq!(
        (report.summary.judge_calls, report.summary.judge_skipped),
        (1, 1)
    );

    // `--judge-all`:檢查沒過也叫評審(拿到逐條標準的結果)。
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");
    let all = ScoreSetup {
        skip_on_check_failure: false,
        ..setup_with("all", Some(&j))
    };
    score(&s.run, &all, options(), t("2026-11-02T00:00:00Z"))
        .await
        .unwrap();
    assert_eq!(fake.calls(), 2);
    assert_eq!(
        outcomes(&lines(&s.run, "all")),
        [
            ("a".into(), 0, ScoreResult::Fail, JudgeStatus::Called),
            ("a".into(), 1, ScoreResult::Pass, JudgeStatus::Called),
        ]
    );
}

#[test]
fn only_a_failed_check_skips_the_judge_not_a_broken_one() {
    // 檢查自己出錯(Error)不是「沒過」:評審說不通過的話,結果會從不完整變成不通過,所以還是要問。
    // 讀資料集時就會擋下壞掉的 regex,流程裡做不出只會 Error 的檢查,所以直接測做決定的函式。
    assert!(!any_check_failed(&[verdict(VerdictResult::Error)]));
    assert!(!any_check_failed(&[verdict(VerdictResult::Pass)]));
    assert!(any_check_failed(&[
        verdict(VerdictResult::Error),
        verdict(VerdictResult::Fail)
    ]));
}

#[tokio::test(start_paused = true)]
async fn a_rubric_without_a_judge_is_refused_before_anything_is_written() {
    let s = setup(
        &[case_line("a", &[]), case_line("b", &["r1"])],
        &[("a", 0, Some("x")), ("b", 0, Some("x"))],
    );

    let err = score(
        &s.run,
        &setup_with("checks", None),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(&err, ScoreError::JudgeRequired { case_id } if case_id == "b"),
        "{err:?}"
    );
    assert!(!s.run.join("scores").exists());
}

#[tokio::test(start_paused = true)]
async fn judging_your_own_answers_is_warned_about() {
    let s = setup(&[case_line("a", &["r1"])], &[("a", 0, Some("x"))]);
    let run = |judge_model: &'static str, served: &'static str, name: &'static str| {
        let run = s.run.clone();
        async move {
            let fake = FakeJudge::new(served);
            let j = judge(&fake, judge_model);
            score(
                &run,
                &setup_with(name, Some(&j)),
                options(),
                t("2026-11-02T00:00:00Z"),
            )
            .await
            .unwrap()
            .warnings
        }
    };

    // 別名相同。
    assert_eq!(
        run("cheap", "other-id", "same-alias").await,
        [ScoreWarning::SelfPreference {
            models: vec!["cheap".into()]
        }]
    );
    // 別名不同,但實際回答的是同一個模型。
    assert_eq!(
        run("haiku", "cheap-id", "same-model").await,
        [ScoreWarning::SelfPreference {
            models: vec!["cheap-id".into()]
        }]
    );
    // 評審的別名剛好是被測模型實際的 id。
    assert_eq!(
        run("cheap-id", "x", "alias-is-id").await,
        [ScoreWarning::SelfPreference {
            models: vec!["cheap-id".into()]
        }]
    );
    assert_eq!(run("default", "judge-id", "different").await, []);
}

#[tokio::test(start_paused = true)]
async fn the_budget_stops_new_judge_calls_and_the_same_name_finishes_the_rest() {
    let cases = [case_line("a", &["r1"]), case_line("b", &[])];
    let answers = [
        ("a", 0, Some("x")),
        ("a", 1, Some("x")),
        ("a", 2, Some("x")),
        ("b", 0, Some("x")),
    ];
    let s = setup(&cases, &answers);
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");
    // 一次一個,預算只夠一次評審(第二次開始前就已經超過)。
    let tight = ExecOptions {
        concurrency: 1,
        budget_usd: Some(PER_JUDGE_CALL / 2.0),
        ..options()
    };

    let first = score(
        &s.run,
        &setup_with("j", Some(&j)),
        tight,
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();

    assert_eq!(fake.calls(), 1);
    // 不需要評審的 b 照樣評完(不花錢);a 只評了一次,剩下兩筆留給接續。
    assert_eq!(
        (
            first.summary.pass,
            first.summary.missing,
            first.summary.judge_calls
        ),
        (2, 2, 1)
    );

    // 同一個名字再打一次:只補沒評的,之前花的也算進預算。
    let still_tight = score(
        &s.run,
        &setup_with("j", Some(&j)),
        tight,
        t("2026-11-02T01:00:00Z"),
    )
    .await
    .unwrap();
    assert_eq!(fake.calls(), 1);
    assert_eq!(still_tight.summary.missing, 2);

    let done = score(
        &s.run,
        &setup_with("j", Some(&j)),
        options(),
        t("2026-11-02T02:00:00Z"),
    )
    .await
    .unwrap();
    assert_eq!(fake.calls(), 3);
    assert_eq!(
        (
            done.summary.pass,
            done.summary.missing,
            done.summary.judge_calls
        ),
        (4, 0, 3)
    );
    assert!(approx(done.summary.cost_usd.unwrap(), 3.0 * PER_JUDGE_CALL));
    // 每一筆回應剛好一行打分,沒有重評。
    assert_eq!(lines(&s.run, "j").len(), 4);
    let meta = score_meta(&s.run, "j").unwrap();
    // 開始時間是第一次打分的時間,結束時間是最後一次。
    assert_eq!(meta.started_at, t("2026-11-02T00:00:00Z"));
    assert_eq!(meta.finished_at, Some(t("2026-11-02T02:00:00Z")));
}

#[tokio::test(start_paused = true)]
async fn a_budget_without_a_judge_price_is_refused() {
    let s = setup(&[case_line("a", &["r1"])], &[("a", 0, Some("x"))]);
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");
    let no_price = ExecOptions {
        budget_usd: Some(1.0),
        price: None,
        ..options()
    };

    let err = score(
        &s.run,
        &setup_with("j", Some(&j)),
        no_price,
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, ScoreError::BudgetWithoutPrice), "{err:?}");
    assert_eq!(fake.calls(), 0);
    assert!(!s.run.join("scores").exists());
    // 只做確定性檢查時沒有錢要花:有預算也不需要價格。
    let s = setup(&[case_line("a", &[])], &[("a", 0, Some("x"))]);
    score(
        &s.run,
        &setup_with("c", None),
        no_price,
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_same_name_with_different_settings_is_refused() {
    let s = setup(&[case_line("a", &["r1"])], &[("a", 0, Some("x"))]);
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");
    score(
        &s.run,
        &setup_with("j", Some(&j)),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();
    let before = std::fs::read(s.run.join("scores/j.jsonl")).unwrap();

    let other = Judge::new(fake.clone(), "sonnet", "high");
    let err = score(
        &s.run,
        &ScoreSetup {
            skip_on_check_failure: false,
            ..setup_with("j", Some(&other))
        },
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap_err();

    let ScoreError::ConfigChanged { name, differences } = &err else {
        panic!("{err:?}")
    };
    assert_eq!(name, "j");
    assert_eq!(
        differences,
        &[
            "judge: default (effort low) → sonnet (effort high)",
            "skip_on_check_failure: true → false"
        ]
    );
    assert_eq!(std::fs::read(s.run.join("scores/j.jsonl")).unwrap(), before);
    assert_eq!(fake.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_changed_rubric_can_be_scored_under_a_new_name_but_a_changed_question_cannot() {
    let original = case_line("a", &["r1"]);
    let s = setup(std::slice::from_ref(&original), &[("a", 0, Some("x"))]);
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");
    score(
        &s.run,
        &setup_with("v1", Some(&j)),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();

    // 改評分標準:舊的回答照樣可以重新打分(不必重付呼叫模型的錢,D-032)。
    std::fs::write(&s.dataset, case_line("a", &["r1", "r2"])).unwrap();
    // 同一個名字不行:評分方式變了。
    let err = score(
        &s.run,
        &setup_with("v1", Some(&j)),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, ScoreError::ConfigChanged { differences, .. }
            if differences == &["the checks, rubric or reference answers changed"]),
        "{err:?}"
    );
    score(
        &s.run,
        &setup_with("v2", Some(&j)),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();
    let v2 = lines(&s.run, "v2");
    assert_eq!(v2[0].verdicts.len(), 3);
    let (c1, c2) = (
        score_meta(&s.run, "v1").unwrap().config.criteria_sha256,
        score_meta(&s.run, "v2").unwrap().config.criteria_sha256,
    );
    assert_eq!(c1.len(), 64);
    assert_ne!(c1, c2);

    // 改題目:舊的回答是另一題的答案,拒絕。
    std::fs::write(&s.dataset, original.replace("q-a", "q-changed")).unwrap();
    let err = score(
        &s.run,
        &setup_with("v3", Some(&j)),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, ScoreError::InputChanged { case_id } if case_id == "a"),
        "{err:?}"
    );
    assert!(!s.run.join("scores/v3.jsonl").exists());
}

#[tokio::test(start_paused = true)]
async fn a_case_removed_from_the_dataset_is_refused() {
    let s = setup(
        &[case_line("a", &[]), case_line("b", &[])],
        &[("a", 0, Some("x")), ("b", 0, Some("x"))],
    );
    std::fs::write(&s.dataset, case_line("a", &[])).unwrap();

    let err = score(
        &s.run,
        &setup_with("c", None),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(&err, ScoreError::CaseMissing { case_id } if case_id == "b"),
        "{err:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_newer_response_for_the_same_sample_is_scored_again() {
    // 執行接續時重做了失敗的呼叫(`--retry-errors`):那筆的舊打分作廢,再打一次分只評新的那筆。
    let cases = [case_line("a", &[]), case_line("b", &[])];
    let s = setup(&cases, &[("a", 0, None), ("b", 0, Some("x"))]);
    score(
        &s.run,
        &setup_with("c", None),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();

    let dir = RunDir::open(&s.run).unwrap();
    append(
        &dir,
        &Dataset::load(&s.dataset).unwrap(),
        &[("a", 0, Some("x"))],
    );
    let report = score(
        &s.run,
        &setup_with("c", None),
        options(),
        t("2026-11-02T01:00:00Z"),
    )
    .await
    .unwrap();

    let got = lines(&s.run, "c");
    assert_eq!(got.len(), 3);
    let last = got.last().unwrap();
    assert_eq!(
        (last.case_id.as_str(), last.response, last.result),
        ("a", 2, ScoreResult::Pass)
    );
    // 摘要以新的那筆為準。
    assert_eq!(
        (
            report.summary.responses,
            report.summary.pass,
            report.summary.call_errors
        ),
        (2, 2, 0)
    );
}

#[tokio::test(start_paused = true)]
async fn a_judge_that_never_answers_times_out_into_error_verdicts() {
    let s = setup(
        &[case_line("a", &["r1", "r2"])],
        &[("a", 0, Some("x HANG"))],
    );
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");

    // 時間是暫停的:逾時不必真的等 300 秒。外面再包一層,實作錯了會「失敗」而不是卡住。
    let report = tokio::time::timeout(
        Duration::from_secs(3600),
        score(
            &s.run,
            &setup_with("j", Some(&j)),
            options(),
            t("2026-11-02T00:00:00Z"),
        ),
    )
    .await
    .expect("scoring should not hang")
    .unwrap();

    let line = &lines(&s.run, "j")[0];
    assert_eq!(line.result, ScoreResult::Incomplete);
    let judged: Vec<(VerdictResult, Option<&str>)> = line
        .verdicts
        .iter()
        .filter(|v| v.scorer == "judge")
        .map(|v| (v.result, v.reason.as_deref()))
        .collect();
    let reason = Some("judge call timed out after 300 s");
    assert_eq!(
        judged,
        [
            (VerdictResult::Error, reason),
            (VerdictResult::Error, reason)
        ]
    );
    assert_eq!(report.summary.incomplete, 1);
}

#[tokio::test(start_paused = true)]
async fn judging_runs_concurrently_up_to_the_limit() {
    // 慢的評審:用 `HANG` 以外的方式很難量並行,所以用逾時來量:兩筆都卡住,並行 2 時總共只等一次逾時。
    let s = setup(
        &[case_line("a", &["r1"])],
        &[("a", 0, Some("x HANG")), ("a", 1, Some("x HANG"))],
    );
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");
    let started = tokio::time::Instant::now();

    score(
        &s.run,
        &setup_with("j", Some(&j)),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();

    assert_eq!(started.elapsed(), Duration::from_secs(300));
    let serial = ExecOptions {
        concurrency: 1,
        ..options()
    };
    let started = tokio::time::Instant::now();
    score(
        &s.run,
        &setup_with("serial", Some(&j)),
        serial,
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(600));
}

#[test]
fn the_estimate_counts_only_judge_calls_and_assumes_every_check_passes() {
    let s = setup(
        &[case_line("a", &["r1"]), case_line("b", &[])],
        &[
            ("a", 0, Some("no x here?")),
            ("a", 1, Some("x")),
            ("a", 2, None),
            ("b", 0, Some("x")),
        ],
    );
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");
    let ds = Dataset::load(&s.dataset).unwrap();
    let dir = RunDir::open(&s.run).unwrap();
    let responses = dir.read_responses().unwrap();
    let worst = |i: usize| {
        let CallResult::Ok { output } = &responses[i].result else {
            panic!()
        };
        j.worst_case_usd(&ds.cases[0], output, PRICE)
    };

    let e = estimate(&s.run, &setup_with("j", Some(&j)), Some(PRICE)).unwrap();

    // a 的兩筆回答(檢查有沒有過,事前不知道,一律算進去);失敗的呼叫與沒有評分標準的 b 不算。
    assert_eq!(e.judge_calls, 2);
    assert!(approx(e.worst_case_usd.unwrap(), worst(0) + worst(1)));
    assert_eq!(
        estimate(&s.run, &setup_with("j", Some(&j)), None)
            .unwrap()
            .worst_case_usd,
        None
    );
    // 估價不呼叫模型、不寫檔案。
    assert_eq!(fake.calls(), 0);
    assert!(!s.run.join("scores").exists());
}

#[test]
fn an_invalid_score_name_is_refused() {
    let s = setup(&[case_line("a", &[])], &[("a", 0, Some("x"))]);
    let err = estimate(&s.run, &setup_with("../escape", None), None).unwrap_err();
    assert!(
        matches!(&err, ScoreError::Store(StoreError::InvalidScoreName(n)) if n == "../escape"),
        "{err:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stale_score_counts_as_missing_until_it_is_redone() {
    // 回應更新了,但預算不夠重評:舊的打分評的是舊回答,不能拿來代表新回答,算成「還沒評」。
    let s = setup(&[case_line("a", &["r1"])], &[("a", 0, Some("x"))]);
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");
    score(
        &s.run,
        &setup_with("j", Some(&j)),
        options(),
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();
    let dir = RunDir::open(&s.run).unwrap();
    append(
        &dir,
        &Dataset::load(&s.dataset).unwrap(),
        &[("a", 0, Some("x BAD"))],
    );
    // 之前已經花了一次評審的錢,預算剛好用完。
    let spent_out = ExecOptions {
        budget_usd: Some(PER_JUDGE_CALL),
        ..options()
    };

    let report = score(
        &s.run,
        &setup_with("j", Some(&j)),
        spent_out,
        t("2026-11-02T01:00:00Z"),
    )
    .await
    .unwrap();

    assert_eq!(fake.calls(), 1);
    assert_eq!(
        (
            report.summary.responses,
            report.summary.pass,
            report.summary.missing
        ),
        (1, 0, 1)
    );
}

#[tokio::test(start_paused = true)]
async fn the_cost_is_unknown_when_the_judge_has_no_price() {
    // 「未知」不能當成 0:沒有價格就算不出評審花了多少(同 `run::summarize`)。
    let s = setup(&[case_line("a", &["r1"])], &[("a", 0, Some("x"))]);
    let fake = FakeJudge::new("judge-id");
    let j = judge(&fake, "default");
    let no_price = ExecOptions {
        price: None,
        ..options()
    };

    let report = score(
        &s.run,
        &setup_with("j", Some(&j)),
        no_price,
        t("2026-11-02T00:00:00Z"),
    )
    .await
    .unwrap();

    assert_eq!(report.summary.judge_calls, 1);
    assert_eq!(report.summary.cost_usd, None);
    assert_eq!(lines(&s.run, "j")[0].cost_usd, None);
    // 用量照記,之後補上價格就能算。
    assert_eq!(report.summary.judge_usage.output_tokens, 100);
}

#[test]
fn the_latest_response_of_each_sample_is_listed_in_file_order() {
    // 打分的順序跟檔案一樣(結果可重現;預算用完時,先評的是檔案前面的)。
    let s = setup(
        &["a", "b", "c", "d", "e"].map(|id| case_line(id, &[])),
        &[
            ("a", 0, None),
            ("b", 0, Some("x")),
            ("c", 0, Some("x")),
            ("a", 0, Some("x")),
            ("d", 0, Some("x")),
            ("e", 0, Some("x")),
            ("c", 0, Some("x")),
        ],
    );
    let responses = RunDir::open(&s.run).unwrap().read_responses().unwrap();

    // a 與 c 各有兩筆,取後面那筆;其餘依檔案順序。
    assert_eq!(latest(&responses), [1, 3, 4, 5, 6]);
}
