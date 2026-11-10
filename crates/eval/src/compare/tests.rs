use std::time::Duration;

use llm::{Content, ProviderMetadata, RecordedError, Response, StopKind, StopReason, Usage};

use super::*;
use crate::case::Output;
use crate::run::{ExecOptions, Summary};
use crate::score::{
    JudgeConfig, JudgeStatus, ScoreConfig, ScoreResult, ScoreSetup, ScoreSummary, Verdict,
    VerdictResult,
};
use crate::stats::{RESAMPLES, SEED};
use crate::store::DatasetInfo;

use VerdictResult::{Fail, Pass};

fn t(s: &str) -> jiff::Timestamp {
    s.parse().unwrap()
}

fn run_meta(model: &str) -> RunMeta {
    RunMeta {
        eval_version: "0.1.0".into(),
        started_at: t("2026-11-01T12:00:00Z"),
        finished_at: Some(t("2026-11-01T12:05:00Z")),
        dataset: DatasetInfo {
            name: "ds".into(),
            path: "/nowhere/ds.jsonl".into(),
            sha256: "ds-sha".into(),
            cases: 4,
        },
        config: RunConfig {
            model: model.into(),
            repeats: 1,
            tags: vec![],
            limit: None,
            label: None,
        },
        git_commit: None,
        summary: Some(Summary {
            planned: 4,
            ok: 4,
            errors: 0,
            missing: 0,
            cost_usd: Some(0.10),
        }),
    }
}

fn score_meta() -> ScoreMeta {
    ScoreMeta {
        eval_version: "0.1.0".into(),
        started_at: t("2026-11-02T00:00:00Z"),
        finished_at: Some(t("2026-11-02T00:01:00Z")),
        config: ScoreConfig {
            judge: Some(JudgeConfig {
                model: "default".into(),
                effort: "low".into(),
            }),
            skip_on_check_failure: true,
            criteria_sha256: "criteria".into(),
        },
        summary: Some(ScoreSummary {
            responses: 4,
            pass: 2,
            fail: 2,
            incomplete: 0,
            call_errors: 0,
            missing: 0,
            judge_calls: 4,
            judge_skipped: 0,
            judge_models: vec!["judge-id".into()],
            judge_usage: Usage::default(),
            cost_usd: Some(0.02),
        }),
    }
}

fn response(case_id: &str, sample: u32, served: &str, latency_ms: u64) -> ResponseLine {
    ResponseLine {
        case_id: case_id.into(),
        sample,
        input_sha256: "in".into(),
        result: CallResult::Ok {
            output: Output::Chat(Response {
                id: None,
                model: served.into(),
                content: vec![Content::Text {
                    text: "answer".into(),
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
        latency_ms,
        cost_usd: None,
    }
}

fn score_line(case_id: &str, sample: u32, verdicts: &[(&str, VerdictResult)]) -> ScoreLine {
    let failed = verdicts.iter().any(|(_, r)| *r == Fail);
    ScoreLine {
        case_id: case_id.into(),
        sample,
        response: 0,
        result: if failed {
            ScoreResult::Fail
        } else {
            ScoreResult::Pass
        },
        verdicts: verdicts
            .iter()
            .map(|(criterion, result)| Verdict {
                scorer: "judge".into(),
                criterion: (*criterion).into(),
                result: *result,
                reason: None,
            })
            .collect(),
        judge: JudgeStatus::Called,
        judge_model: None,
        judge_usage: None,
        cost_usd: None,
    }
}

/// 一行判決:(標準, 結果)。
type Verdicts<'a> = &'a [(&'a str, VerdictResult)];

/// 一邊:每個 (案例, 抽樣, 判決)。延遲依序是 100、200、……毫秒。
fn side(model: &str, served: &str, rows: &[(&str, u32, Verdicts<'_>)]) -> RunScores {
    RunScores {
        path: format!("/runs/{model}").into(),
        run: run_meta(model),
        responses: (1..)
            .zip(rows)
            .map(|(n, (id, sample, _))| response(id, *sample, served, n * 100))
            .collect(),
        score_name: "j".into(),
        scores: score_meta(),
        lines: rows
            .iter()
            .map(|(id, sample, verdicts)| score_line(id, *sample, verdicts))
            .collect(),
        tags: BTreeMap::new(),
        tags_unavailable: None,
    }
}

/// 一邊,每個案例一次抽樣、一條標準:`true` = 通過。
fn simple(model: &str, outcomes: &[(&str, bool)]) -> RunScores {
    let verdicts: Vec<[(&str, VerdictResult); 1]> = outcomes
        .iter()
        .map(|(_, ok)| [("c", if *ok { Pass } else { Fail })])
        .collect();
    let rows: Vec<(&str, u32, Verdicts<'_>)> = outcomes
        .iter()
        .zip(&verdicts)
        .map(|((id, _), v)| (*id, 0, v.as_slice()))
        .collect();
    side(model, &format!("{model}-id"), &rows)
}

#[test]
fn different_scoring_settings_are_refused_unless_allowed() {
    let a = simple("cheap", &[("x", true)]);
    let mut b = simple("default", &[("x", true)]);
    b.scores.eval_version = "0.2.0".into();
    b.scores.config.skip_on_check_failure = false;
    b.scores.config.criteria_sha256 = "other".into();
    if let Some(judge) = &mut b.scores.config.judge {
        judge.effort = "high".into();
    }
    let expected = [
        "judge: default (effort low) → default (effort high)",
        "skip_on_check_failure: true → false",
        "the checks, rubric or reference answers changed",
        "eval version: 0.1.0 → 0.2.0",
    ];

    let err = compare(&a, &b, false).unwrap_err();
    let CompareError::Mismatch { differences } = &err else {
        panic!("{err:?}")
    };
    assert_eq!(differences, &expected);

    // `--allow-mismatch`:照樣比,差異放在報告裡(最上面標示「不可直接比較」)。
    let report = compare(&a, &b, true).unwrap();
    assert_eq!(report.mismatches, expected);
    assert!(!report.noise_floor);
    let text = report.to_string();
    assert!(
        text.contains("NOT DIRECTLY COMPARABLE") && text.contains(expected[3]),
        "{text}"
    );

    // 執行設定完全一樣,只有打分設定不同:也不是雜訊底線(分數的意思不同)。
    let mut rescored = simple("cheap", &[("x", true)]);
    rescored.scores.eval_version = "0.2.0".into();
    let report = compare(&a, &rescored, true).unwrap();
    assert!(!report.noise_floor);
}

#[test]
fn identical_settings_are_labelled_as_the_noise_floor() {
    let same = || simple("cheap", &[("x", true)]);
    let report = compare(&same(), &same(), false).unwrap();
    assert!(report.noise_floor);
    assert!(report.to_string().contains("NOISE FLOOR"), "{report}");

    // label 只是給人看的說明,不算設定不同。
    let mut labelled = same();
    labelled.run.config.label = Some("second try".into());
    assert!(compare(&same(), &labelled, false).unwrap().noise_floor);

    // 換了模型、實際回答的模型不同(例如別名表改了)、資料集內容不同:都不是雜訊底線。
    let other_model = simple("default", &[("x", true)]);
    assert!(!compare(&same(), &other_model, false).unwrap().noise_floor);

    let mut other_served = same();
    other_served.responses[0] = response("x", 0, "new-id", 100);
    assert!(!compare(&same(), &other_served, false).unwrap().noise_floor);

    let mut other_dataset = same();
    other_dataset.run.dataset.sha256 = "changed".into();
    let report = compare(&same(), &other_dataset, false).unwrap();
    assert!(!report.noise_floor);
    assert!(report.warnings.contains(&CompareWarning::DatasetChanged {
        a: "ds-sha".into(),
        b: "changed".into()
    }));
    assert!(!report.to_string().contains("NOISE FLOOR"));
}

#[test]
fn both_measures_are_paired_by_case_with_a_bootstrap_interval() {
    let a = side(
        "cheap",
        "cheap-id",
        &[
            ("x", 0, &[("c", Pass), ("d", Fail)]),
            ("y", 0, &[("c", Pass), ("d", Pass)]),
            ("only-a", 0, &[("c", Pass)]),
        ],
    );
    let b = side(
        "default",
        "default-id",
        &[
            ("x", 0, &[("c", Pass), ("d", Pass)]),
            ("y", 0, &[("c", Fail), ("d", Pass)]),
            ("only-b", 0, &[("c", Pass)]),
        ],
    );

    let report = compare(&a, &b, false).unwrap();

    let (ca, cb) = (stats::case_scores(&a.lines), stats::case_scores(&b.lines));
    for (comparison, measure) in [
        (&report.all_pass, Measure::AllPass),
        (&report.partial, Measure::Partial),
    ] {
        let pairing = stats::pair(&ca, &cb, measure);
        assert_eq!(
            comparison.interval,
            stats::paired_bootstrap(&pairing.values(), RESAMPLES, SEED)
        );
        assert_eq!(comparison.pairing, pairing);
    }
    assert_eq!(report.all_pass.pairing.only_a, ["only-a"]);
    // 報告的 A、B 兩欄:配對案例(x、y)上的平均,不含只在一邊的案例。
    // x:A 0/1 → B 1/1;y:A 1/1 → B 0/1。部分得分:x 0.5 → 1.0;y 1.0 → 0.5。
    assert_eq!(report.all_pass.paired_means(), (Some(0.5), Some(0.5)));
    assert_eq!(report.partial.paired_means(), (Some(0.75), Some(0.75)));
    let empty = Comparison {
        pairing: Pairing::default(),
        interval: None,
    };
    assert_eq!(empty.paired_means(), (None, None));
    assert_eq!(report.all_pass.pairing.only_b, ["only-b"]);
    assert_eq!(report.a.stats, stats::stats(&a.lines));
    assert_eq!(report.b.stats, stats::stats(&b.lines));
    assert_eq!(
        (report.a.model.as_str(), report.a.served.as_slice()),
        ("cheap", ["cheap-id".to_owned()].as_slice())
    );
    assert_eq!(report.score_name, "j");
    assert_eq!(report.scores, a.scores.config);
}

#[test]
fn flipped_cases_are_listed_biggest_change_first() {
    // id 的順序(a、b、c)跟變化幅度的順序不同,才看得出有沒有依幅度排序。
    let a = side(
        "cheap",
        "cheap-id",
        &[
            ("a-half", 0, &[("c", Pass)]),
            ("a-half", 1, &[("c", Pass)]),
            ("b-full", 0, &[("c", Pass)]),
            ("c-better", 0, &[("c", Fail)]),
            ("d-same", 0, &[("c", Pass)]),
        ],
    );
    let b = side(
        "default",
        "default-id",
        &[
            ("a-half", 0, &[("c", Pass)]),
            ("a-half", 1, &[("c", Fail)]),
            ("b-full", 0, &[("c", Fail)]),
            ("c-better", 0, &[("c", Pass)]),
            ("d-same", 0, &[("c", Pass)]),
        ],
    );

    let report = compare(&a, &b, false).unwrap();

    let flips = |list: &[Flip]| -> Vec<(String, f64, f64)> {
        list.iter().map(|f| (f.case_id.clone(), f.a, f.b)).collect()
    };
    assert_eq!(
        flips(&report.regressed),
        [("b-full".into(), 1.0, 0.0), ("a-half".into(), 1.0, 0.5)]
    );
    assert_eq!(flips(&report.improved), [("c-better".into(), 0.0, 1.0)]);
}

#[test]
fn results_are_grouped_by_tag() {
    let mut a = simple("cheap", &[("x", true), ("y", false), ("z", false)]);
    let b = simple("default", &[("x", true), ("y", true), ("z", false)]);
    a.tags = [
        ("x", vec!["rust"]),
        ("y", vec!["rust", "web"]),
        ("z", vec!["go"]),
    ]
    .into_iter()
    .map(|(id, tags)| (id.to_owned(), tags.into_iter().map(str::to_owned).collect()))
    .collect();

    let report = compare(&a, &b, false).unwrap();

    let rows: Vec<(&str, usize, Option<f64>, Option<f64>)> = report
        .by_tag
        .iter()
        .map(|r| (r.tag.as_str(), r.all_pass.cases, r.all_pass.a, r.all_pass.b))
        .collect();
    assert_eq!(
        rows,
        [
            ("go", 1, Some(0.0), Some(0.0)),
            ("rust", 2, Some(0.5), Some(1.0)),
            ("web", 1, Some(0.0), Some(1.0)),
        ]
    );
    assert_eq!(
        report.by_tag[1].partial,
        Means {
            cases: 2,
            a: Some(0.5),
            b: Some(1.0)
        }
    );
}

#[test]
fn results_are_grouped_by_tag_using_either_dataset() {
    // 案例的 tag 以 A 的資料集為準;A 的資料集沒有這個案例(例如之後才加的)時,看 B 的。
    let a = simple("cheap", &[("x", true), ("y", false)]);
    let mut b = simple("default", &[("x", true), ("y", true)]);
    b.tags = [("y".to_owned(), vec!["new".to_owned()])]
        .into_iter()
        .collect();

    let report = compare(&a, &b, false).unwrap();

    let rows: Vec<(&str, usize)> = report
        .by_tag
        .iter()
        .map(|r| (r.tag.as_str(), r.all_pass.cases))
        .collect();
    assert_eq!(rows, [("new", 1)]);
}

#[test]
fn without_a_readable_dataset_there_are_no_tag_groups() {
    let mut a = simple("cheap", &[("x", true)]);
    a.tags_unavailable = Some("no such file".into());
    let b = simple("default", &[("x", true)]);

    let report = compare(&a, &b, false).unwrap();

    assert!(report.by_tag.is_empty());
    assert!(report.warnings.contains(&CompareWarning::TagsUnavailable {
        reason: "no such file".into()
    }));
}

#[test]
fn cost_adds_the_run_and_the_scoring_and_is_unknown_if_either_is() {
    let a = simple(
        "cheap",
        &[("x", true), ("y", true), ("z", true), ("w", true)],
    );
    let mut b = simple(
        "default",
        &[("x", true), ("y", true), ("z", true), ("w", true)],
    );
    if let Some(summary) = &mut b.scores.summary {
        summary.cost_usd = None;
    }

    let report = compare(&a, &b, false).unwrap();

    let total = report.a.total_cost_usd().unwrap();
    assert!((total - 0.12).abs() < 1e-12, "{total}");
    let per_case = report.a.cost_per_case_usd().unwrap();
    assert!((per_case - 0.03).abs() < 1e-12, "{per_case}");
    assert_eq!(report.b.total_cost_usd(), None);
    assert_eq!(report.b.cost_per_case_usd(), None);
    assert_eq!(
        (report.b.run_cost_usd, report.b.score_cost_usd),
        (Some(0.10), None)
    );
}

#[test]
fn latency_percentiles_use_successful_calls_only() {
    let ids: Vec<String> = (0..10).map(|i| format!("c{i}")).collect();
    let outcomes: Vec<(&str, bool)> = ids.iter().map(|id| (id.as_str(), true)).collect();
    let mut a = simple("cheap", &outcomes);
    // 失敗的呼叫(例如逾時)不算進延遲:它會把數字拉歪,錯誤率另外列。
    a.responses.push(ResponseLine {
        case_id: "failed".into(),
        sample: 0,
        input_sha256: "in".into(),
        result: CallResult::Error {
            error: RecordedError {
                kind: "timeout".into(),
                status: None,
                message: String::new(),
            },
        },
        latency_ms: 300_000,
        cost_usd: None,
    });
    let b = simple("default", &outcomes);

    let report = compare(&a, &b, false).unwrap();

    // 延遲是 100、200、……、1000 毫秒。
    assert_eq!(
        (report.a.latency_p50_ms, report.a.latency_p95_ms),
        (Some(500), Some(1000))
    );
}

#[test]
fn percentiles_use_the_nearest_rank() {
    assert_eq!(percentile(&[], 50), None);
    assert_eq!(percentile(&[7], 50), Some(7));
    assert_eq!(percentile(&[7], 95), Some(7));
    // ⌈0.5 × 4⌉ = 第 2 個;⌈0.95 × 4⌉ = 第 4 個。
    assert_eq!(percentile(&[1, 2, 3, 4], 50), Some(2));
    assert_eq!(percentile(&[1, 2, 3, 4], 95), Some(4));
    // ⌈0.95 × 20⌉ = 第 19 個。
    let twenty: Vec<u64> = (1..=20).collect();
    assert_eq!(percentile(&twenty, 95), Some(19));
}

#[test]
fn incomplete_runs_and_scores_and_few_cases_are_warned_about() {
    let mut a = simple("cheap", &[("x", true)]);
    if let Some(summary) = &mut a.run.summary {
        summary.missing = 2;
    }
    let mut b = simple("default", &[("x", true)]);
    if let Some(summary) = &mut b.scores.summary {
        summary.missing = 1;
        summary.incomplete = 3;
    }

    let report = compare(&a, &b, false).unwrap();

    assert_eq!(
        report.warnings,
        [
            CompareWarning::RunIncomplete {
                side: 'A',
                missing: 2
            },
            CompareWarning::ScoresIncomplete {
                side: 'B',
                missing: 1,
                incomplete: 3
            },
            CompareWarning::FewCases { cases: 1 },
        ]
    );
}

#[test]
fn scorer_errors_alone_make_the_scores_incomplete() {
    let a = simple("cheap", &[("x", true)]);
    let mut b = simple("default", &[("x", true)]);
    if let Some(summary) = &mut b.scores.summary {
        summary.incomplete = 2;
    }
    let report = compare(&a, &b, false).unwrap();
    assert!(report.warnings.contains(&CompareWarning::ScoresIncomplete {
        side: 'B',
        missing: 0,
        incomplete: 2
    }));
}

#[test]
fn thirty_paired_cases_are_enough() {
    let ids: Vec<String> = (0..30).map(|i| format!("c{i}")).collect();
    let outcomes: Vec<(&str, bool)> = ids.iter().map(|id| (id.as_str(), true)).collect();
    let report = compare(
        &simple("cheap", &outcomes),
        &simple("default", &outcomes),
        false,
    )
    .unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[test]
fn only_criteria_whose_rate_changed_are_listed() {
    let a = side(
        "cheap",
        "cheap-id",
        &[
            ("x", 0, &[("c", Pass), ("d", Pass)]),
            ("y", 0, &[("c", Pass)]),
        ],
    );
    let b = side(
        "default",
        "default-id",
        &[
            ("x", 0, &[("c", Pass), ("d", Fail)]),
            ("y", 0, &[("e", Pass)]),
        ],
    );

    let report = compare(&a, &b, false).unwrap();

    // x 的 d 變了;x 的 c 沒變;y 的 c、e 只在一邊。
    assert_eq!(
        report.criteria,
        [CriterionRow {
            case_id: "x".into(),
            scorer: "judge".into(),
            criterion: "d".into(),
            a: 1.0,
            b: 0.0
        }]
    );
}

#[test]
fn the_report_reads_well() {
    let mut a = side(
        "cheap",
        "claude-haiku-5-5",
        &[
            ("ownership-001", 0, &[("c", Pass), ("d", Fail)]),
            ("ownership-002", 0, &[("c", Pass), ("d", Pass)]),
            ("lifetimes-001", 0, &[("c", Fail), ("d", Fail)]),
            ("only-a", 0, &[("c", Pass)]),
        ],
    );
    a.run.config.label = Some("old system prompt".into());
    a.tags = [
        ("ownership-001", "ownership"),
        ("ownership-002", "ownership"),
        ("lifetimes-001", "lifetimes"),
    ]
    .into_iter()
    .map(|(id, tag)| (id.to_owned(), vec![tag.to_owned()]))
    .collect();
    let mut b = side(
        "default",
        "claude-sonnet-5-5",
        &[
            ("ownership-001", 0, &[("c", Pass), ("d", Pass)]),
            ("ownership-002", 0, &[("c", Pass), ("d", Pass)]),
            ("lifetimes-001", 0, &[("c", Pass), ("d", Fail)]),
        ],
    );
    b.path = "/runs/default".into();
    if let Some(summary) = &mut b.scores.summary {
        summary.cost_usd = Some(0.05);
    }

    let report = compare(&a, &b, false).unwrap();

    insta::assert_snapshot!(report.to_string());
}

/// 真的執行目錄:用 `store` 寫出執行、用 `score::score` 打分,再 `load` 回來。
mod load_from_disk {
    use super::*;

    fn case_line(id: &str, tags: &[&str]) -> String {
        format!(
            r#"{{"id":"{id}","tags":{},"input":{{"type":"chat","max_tokens":64,"messages":[{{"role":"user","content":[{{"type":"text","text":"q-{id}"}}]}}]}},"checks":[{{"type":"contains","value":"x"}}]}}"#,
            serde_json::to_string(tags).unwrap()
        )
    }

    fn append(dir: &RunDir, ds: &Dataset, id: &str, sample: u32, text: &str) {
        let case = ds.cases.iter().find(|c| c.id == id).unwrap();
        let mut line = response(id, sample, "cheap-id", 10);
        line.input_sha256 = case.input_sha256();
        if let CallResult::Ok {
            output: Output::Chat(r),
        } = &mut line.result
        {
            r.content = vec![Content::Text {
                text: text.into(),
                meta: ProviderMetadata::default(),
            }];
        }
        dir.appender().unwrap().append(&line).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn load_keeps_only_scores_of_the_latest_responses() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ds.jsonl");
        std::fs::write(
            &path,
            [case_line("a", &["rust"]), case_line("b", &[])].join("\n"),
        )
        .unwrap();
        let ds = Dataset::load(&path).unwrap();
        let mut meta = run_meta("cheap");
        meta.dataset.path = path.clone();
        meta.dataset.sha256 = ds.sha256.clone();
        let dir = RunDir::create(&tmp.path().join("runs"), &meta).unwrap();
        append(&dir, &ds, "a", 0, "x");
        append(&dir, &ds, "b", 0, "no");
        let setup = ScoreSetup {
            name: "checks",
            judge: None,
            skip_on_check_failure: true,
        };
        let options = ExecOptions {
            concurrency: 1,
            timeout: Duration::from_secs(10),
            budget_usd: None,
            price: None,
        };
        crate::score::score(dir.path(), &setup, options, t("2026-11-02T00:00:00Z"))
            .await
            .unwrap();
        // b 後來重做了(還沒重新打分):舊的打分作廢,不能拿來代表新的回答。
        append(&dir, &ds, "b", 0, "x now");

        let loaded = load(dir.path(), "checks").unwrap();

        assert_eq!(loaded.path, dir.path());
        assert_eq!(loaded.score_name, "checks");
        assert_eq!(loaded.responses.len(), 2);
        let keys: Vec<(&str, u32)> = loaded
            .lines
            .iter()
            .map(|l| (l.case_id.as_str(), l.sample))
            .collect();
        assert_eq!(keys, [("a", 0)]);
        assert_eq!(loaded.tags.get("a"), Some(&vec!["rust".to_owned()]));
        assert_eq!(loaded.tags.get("b"), Some(&vec![]));
        assert_eq!(loaded.tags_unavailable, None);
        assert_eq!(loaded.scores.config.judge, None);

        // 沒有這份打分。
        let err = load(dir.path(), "nope").unwrap_err();
        assert!(
            matches!(&err, CompareError::ScoresMissing { name, .. } if name == "nope"),
            "{err:?}"
        );

        // 資料集不見了:照樣讀得出結果,只是沒有 tag(原因寫在 `tags_unavailable`)。
        std::fs::remove_file(&path).unwrap();
        let loaded = load(dir.path(), "checks").unwrap();
        assert!(loaded.tags.is_empty());
        assert!(loaded.tags_unavailable.is_some());
    }
}

#[test]
fn cost_per_case_is_unknown_without_cases() {
    // (cargo-mutants:`cases > 0` 換成 `>=` 時,0 個案例會得到 $inf 而沒有測試失敗。)
    let mut a = simple("cheap", &[]);
    a.run.summary = None;
    let mut report = compare(&a, &simple("default", &[]), false).unwrap();
    report.a.run_cost_usd = Some(0.1);
    report.a.score_cost_usd = Some(0.1);
    assert!(report.a.total_cost_usd().is_some());
    assert_eq!(report.a.cost_per_case_usd(), None);
}

#[test]
fn unscored_responses_alone_make_the_scores_incomplete() {
    // (cargo-mutants:`missing > 0` 換成 `< 0` 時沒有測試失敗:之前的測試同時設了 incomplete。)
    let a = simple("cheap", &[("x", true)]);
    let mut b = simple("default", &[("x", true)]);
    if let Some(summary) = &mut b.scores.summary {
        summary.missing = 1;
    }
    let report = compare(&a, &b, false).unwrap();
    assert!(report.warnings.contains(&CompareWarning::ScoresIncomplete {
        side: 'B',
        missing: 1,
        incomplete: 0
    }));
}

#[test]
fn flips_are_ordered_by_the_size_of_the_change_with_ties_by_id() {
    // 四次抽樣,案例分數是 1/4 的倍數:變化幅度各不相同,而且跟 id 的順序不同。
    // (cargo-mutants:排序比較式裡的 `-` 換成 `/`、`+` 時,只有兩個案例的測試看不出來。)
    let samples = |pattern: [bool; 4]| -> Vec<VerdictResult> {
        pattern
            .iter()
            .map(|ok| if *ok { Pass } else { Fail })
            .collect()
    };
    let build = |model: &str, rows: &[(&str, [bool; 4])]| {
        type Row<'a> = (&'a str, u32, [(&'a str, VerdictResult); 1]);
        let verdicts: Vec<Row<'_>> = rows
            .iter()
            .flat_map(|(id, pattern)| {
                samples(*pattern)
                    .into_iter()
                    .zip(0..)
                    .map(move |(v, n)| (*id, n, [("c", v)]))
            })
            .collect();
        let rows: Vec<(&str, u32, Verdicts<'_>)> = verdicts
            .iter()
            .map(|(id, n, v)| (*id, *n, v.as_slice()))
            .collect();
        side(model, &format!("{model}-id"), &rows)
    };
    let (t, f) = (true, false);
    let a = build(
        "cheap",
        &[
            ("a", [t, t, t, t]), // 1.00 → 0.75
            ("b", [t, t, t, t]), // 1.00 → 0.00
            ("c", [t, t, t, t]), // 1.00 → 0.50
            ("d", [t, t, f, f]), // 0.50 → 0.25(跟 a 一樣大,依 id)
            ("e", [f, f, f, f]), // 0.00 → 0.25
            ("g", [f, f, f, f]), // 0.00 → 1.00
        ],
    );
    let b = build(
        "default",
        &[
            ("a", [t, t, t, f]),
            ("b", [f, f, f, f]),
            ("c", [t, t, f, f]),
            ("d", [t, f, f, f]),
            ("e", [t, f, f, f]),
            ("g", [t, t, t, t]),
        ],
    );

    let report = compare(&a, &b, false).unwrap();

    let ids = |list: &[Flip]| -> Vec<String> { list.iter().map(|x| x.case_id.clone()).collect() };
    assert_eq!(ids(&report.regressed), ["b", "c", "a", "d"]);
    assert_eq!(ids(&report.improved), ["g", "e"]);
}

#[test]
fn a_clear_difference_says_which_side_is_better() {
    // (cargo-mutants:`diff > 0.0` 換成 `==`、`<`、`>=` 時沒有測試失敗:報告裡從沒出現過有差異的情況。)
    let interval = |low: f64, high: f64| Interval {
        cases: 50,
        diff: (low + high) / 2.0,
        low,
        high,
        resamples: RESAMPLES,
        seed: SEED,
    };
    assert_eq!(verdict(&interval(0.1, 0.3)), "B is better");
    assert_eq!(verdict(&interval(-0.3, -0.1)), "B is worse");
    assert_eq!(verdict(&interval(-0.1, 0.3)), "no detectable difference");
    // 剛好碰到 0 也是「看不出差異」(同 `Interval::excludes_zero`)。
    assert_eq!(verdict(&interval(0.0, 0.3)), "no detectable difference");
    assert_eq!(verdict(&interval(-0.3, 0.0)), "no detectable difference");
}

#[test]
fn the_call_error_rate_is_shown_as_a_fraction_and_a_percentage() {
    // (cargo-mutants:錯誤數一直是 0 時,`/` 換成 `*`、`%` 都一樣是 0。)
    let mut s = stats::stats(&[]);
    assert_eq!(errors(&s), "0/0 (-)");
    s.responses = 4;
    s.call_errors = 1;
    assert_eq!(errors(&s), "1/4 (25.0%)");
}

#[test]
fn the_scoring_to_compare_is_the_one_both_runs_have() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("runs");
    let mut meta = run_meta("cheap");
    let a = RunDir::create(&root, &meta).unwrap();
    meta.config.model = "default".into();
    let b = RunDir::create(&root, &meta).unwrap();
    let write =
        |dir: &RunDir, name: &str| dir.scores(name).unwrap().write_meta(&score_meta()).unwrap();

    // 兩邊都沒有。
    let err = common_score_name(a.path(), b.path()).unwrap_err();
    assert!(
        matches!(&err, CompareError::NoCommonScores { a, b } if a.is_empty() && b.is_empty()),
        "{err:?}"
    );
    assert!(
        err.to_string().contains("A has: none; B has: none"),
        "{err}"
    );

    // 各有一份,但名字不同:不猜。
    write(&a, "sonnet");
    write(&b, "haiku");
    let err = common_score_name(a.path(), b.path()).unwrap_err();
    assert!(
        matches!(&err, CompareError::NoCommonScores { a, b } if a == &["sonnet"] && b == &["haiku"]),
        "{err:?}"
    );

    // 剛好一份共同的。
    write(&b, "sonnet");
    assert_eq!(common_score_name(a.path(), b.path()).unwrap(), "sonnet");

    // 不只一份:請使用者指定。
    write(&a, "checks");
    write(&b, "checks");
    let err = common_score_name(a.path(), b.path()).unwrap_err();
    assert!(
        matches!(&err, CompareError::AmbiguousScores { common } if common == &["checks", "sonnet"]),
        "{err:?}"
    );
}

#[test]
fn the_header_says_how_the_judge_was_used() {
    let header = |judge: bool, skip: bool| {
        let mut a = simple("cheap", &[("x", true)]);
        let mut b = simple("default", &[("x", true)]);
        for side in [&mut a, &mut b] {
            if !judge {
                side.scores.config.judge = None;
            }
            side.scores.config.skip_on_check_failure = skip;
        }
        let report = compare(&a, &b, false).unwrap().to_string();
        report
            .lines()
            .find(|l| l.contains("scores `j`"))
            .unwrap()
            .trim()
            .to_owned()
    };
    assert_eq!(
        header(true, true),
        "scores `j`: judge default (effort low), judge skipped when a check fails"
    );
    assert_eq!(
        header(true, false),
        "scores `j`: judge default (effort low), judge on every answer"
    );
    // 沒有評審:「跳不跳過評審」不適用。
    assert_eq!(header(false, true), "scores `j`: checks only");
}
