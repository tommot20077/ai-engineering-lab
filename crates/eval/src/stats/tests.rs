use super::*;
use crate::score::{JudgeStatus, Verdict};

use VerdictResult::{Error, Fail, Pass, Skipped};

/// 一行打分。`verdicts`:(標準, 結果)。
fn line(
    case_id: &str,
    sample: u32,
    result: ScoreResult,
    verdicts: &[(&str, VerdictResult)],
) -> ScoreLine {
    ScoreLine {
        case_id: case_id.into(),
        sample,
        response: 0,
        result,
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

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-12
}

#[test]
fn split_mix_64_matches_the_reference_sequence() {
    // 參考實作(Vigna)種子 0 的前三個數。數字一樣,才能說「同樣的種子永遠得到同樣的數列」。
    let mut rng = SplitMix64(0);
    let got: Vec<u64> = (0..3).map(|_| rng.next_u64()).collect();
    assert_eq!(
        got,
        [
            0xe220_a839_7b1d_cdaf,
            0x6e78_9e6a_a1b9_65f4,
            0x06c4_5d18_8009_454f
        ]
    );
}

#[test]
fn below_stays_in_range_and_reaches_every_value() {
    let mut rng = SplitMix64(42);
    let mut seen = [0usize; 3];
    for _ in 0..3000 {
        let i = rng.below(3);
        assert!(i < 3, "{i}");
        if let Some(count) = seen.get_mut(i) {
            *count += 1;
        }
    }
    // 大約各 1000 次:三個值都會出現,也沒有明顯偏向哪一個。
    assert!(seen.iter().all(|&c| (900..1100).contains(&c)), "{seen:?}");
}

#[test]
fn a_response_is_scored_on_both_measures() {
    let score = |result, verdicts: &[(&str, VerdictResult)]| {
        response_score(&line("a", 0, result, verdicts))
    };

    assert_eq!(
        score(ScoreResult::Pass, &[("c1", Pass), ("c2", Pass)]),
        ResponseScore {
            all_pass: Some(true),
            partial: Some(1.0)
        }
    );
    // 進步(1/4 → 3/4)在全部通過率上看不到,部分得分看得到。
    assert_eq!(
        score(
            ScoreResult::Fail,
            &[("c1", Pass), ("c2", Pass), ("c3", Pass), ("c4", Fail)]
        ),
        ResponseScore {
            all_pass: Some(false),
            partial: Some(0.75)
        }
    );
    // Error 不列入分母,不算 0 分。
    assert_eq!(
        score(ScoreResult::Incomplete, &[("c1", Pass), ("c2", Error)]),
        ResponseScore {
            all_pass: None,
            partial: Some(1.0)
        }
    );
    // Skipped(沒評)算 0 分:檢查沒過、沒叫評審的那兩條。
    assert_eq!(
        score(
            ScoreResult::Fail,
            &[("c1", Pass), ("c2", Fail), ("r1", Skipped), ("r2", Skipped)]
        ),
        ResponseScore {
            all_pass: Some(false),
            partial: Some(0.25)
        }
    );
    // 呼叫失敗:全部 Skipped → 0 分,也算沒通過。
    assert_eq!(
        score(ScoreResult::CallError, &[("c1", Skipped)]),
        ResponseScore {
            all_pass: Some(false),
            partial: Some(0.0)
        }
    );
    // 全部都是 Error:兩個指標都算不出來。
    assert_eq!(
        score(ScoreResult::Incomplete, &[("c1", Error)]),
        ResponseScore {
            all_pass: None,
            partial: None
        }
    );
}

#[test]
fn case_scores_average_the_samples_that_can_be_scored() {
    let lines = [
        line("b", 0, ScoreResult::Pass, &[("c", Pass)]),
        line("a", 0, ScoreResult::Pass, &[("c", Pass), ("d", Pass)]),
        line("a", 1, ScoreResult::Fail, &[("c", Pass), ("d", Fail)]),
        // 不知道算不算通過:不列入 a 的全部通過率,但部分得分算得出來(1/1)。
        line(
            "a",
            2,
            ScoreResult::Incomplete,
            &[("c", Pass), ("d", Error)],
        ),
        line("c", 0, ScoreResult::Incomplete, &[("c", Error)]),
    ];

    let got = case_scores(&lines);

    assert_eq!(
        got,
        [
            CaseScore {
                case_id: "a".into(),
                samples: 3,
                all_pass: Some(0.5),
                partial: Some((1.0 + 0.5 + 1.0) / 3.0),
            },
            CaseScore {
                case_id: "b".into(),
                samples: 1,
                all_pass: Some(1.0),
                partial: Some(1.0)
            },
            CaseScore {
                case_id: "c".into(),
                samples: 1,
                all_pass: None,
                partial: None
            },
        ]
    );
}

#[test]
fn stats_average_over_cases_and_count_each_criterion() {
    let lines = [
        line("a", 0, ScoreResult::Pass, &[("c", Pass), ("r", Pass)]),
        line("a", 1, ScoreResult::Fail, &[("c", Fail), ("r", Skipped)]),
        line("b", 0, ScoreResult::CallError, &[("c", Skipped)]),
        line(
            "b",
            1,
            ScoreResult::Incomplete,
            &[("c", Pass), ("r2", Error)],
        ),
    ];

    let s = stats(&lines);

    assert_eq!(
        (s.cases, s.responses, s.incomplete, s.call_errors),
        (2, 4, 1, 1)
    );
    // 案例 a 全部通過率 0.5、b 是 0.0(b/1 不知道,不列入):平均 0.25。每個案例一樣重,不是每筆回應一樣重。
    assert_eq!(
        s.all_pass,
        Metric {
            mean: Some(0.25),
            cases: 2
        }
    );
    // a:(1 + 0) / 2 = 0.5;b:(0 + 1) / 2 = 0.5。
    assert_eq!(
        s.partial,
        Metric {
            mean: Some(0.5),
            cases: 2
        }
    );
    let rates: Vec<(&str, &str, usize, usize, usize)> = s
        .criteria
        .iter()
        .map(|c| {
            (
                c.case_id.as_str(),
                c.criterion.as_str(),
                c.pass,
                c.counted,
                c.errors,
            )
        })
        .collect();
    assert_eq!(
        rates,
        [
            ("a", "c", 1, 2, 0),
            // Skipped 在分母裡(算 0 分)。
            ("a", "r", 1, 2, 0),
            ("b", "c", 1, 2, 0),
            // Error 不在分母裡,另外計數。
            ("b", "r2", 0, 0, 1),
        ]
    );
    assert_eq!(s.criteria[1].rate(), Some(0.5));
    assert_eq!(s.criteria[3].rate(), None);
    assert!(s.criteria.iter().all(|c| c.scorer == "judge"));
}

#[test]
fn stats_of_nothing_are_unknown_not_zero() {
    let s = stats(&[]);
    assert_eq!(
        s.all_pass,
        Metric {
            mean: None,
            cases: 0
        }
    );
    assert_eq!((s.cases, s.responses), (0, 0));
}

fn case(id: &str, all_pass: Option<f64>, partial: Option<f64>) -> CaseScore {
    CaseScore {
        case_id: id.into(),
        samples: 1,
        all_pass,
        partial,
    }
}

#[test]
fn pairing_matches_cases_by_id_and_lists_the_rest() {
    let a = [
        case("x", Some(0.0), Some(0.5)),
        case("only-a", Some(1.0), Some(1.0)),
        case("w", Some(1.0), None),
        case("y", Some(1.0), Some(1.0)),
    ];
    let b = [
        case("y", Some(0.5), Some(0.2)),
        case("only-b", Some(1.0), Some(1.0)),
        case("x", Some(1.0), Some(0.7)),
        case("w", None, Some(1.0)),
    ];

    let all = pair(&a, &b, Measure::AllPass);
    assert_eq!(
        all.pairs,
        [("x".to_owned(), 0.0, 1.0), ("y".to_owned(), 1.0, 0.5)]
    );
    assert_eq!(all.only_a, ["only-a"]);
    assert_eq!(all.only_b, ["only-b"]);
    assert_eq!(all.unscored, ["w"]);
    assert_eq!(all.values(), [(0.0, 1.0), (1.0, 0.5)]);

    // 換一個指標,用的是另一個欄位。
    let partial = pair(&a, &b, Measure::Partial);
    assert_eq!(
        partial.pairs,
        [("x".to_owned(), 0.5, 0.7), ("y".to_owned(), 1.0, 0.2)]
    );
    assert_eq!(partial.unscored, ["w"]);
}

#[test]
fn a_constant_difference_has_a_zero_width_interval() {
    let pairs = [(0.2, 0.5), (0.4, 0.7), (0.0, 0.3)];
    let i = paired_bootstrap(&pairs, 1000, SEED).unwrap();
    assert!(
        approx(i.diff, 0.3) && approx(i.low, 0.3) && approx(i.high, 0.3),
        "{i:?}"
    );
    assert_eq!((i.cases, i.resamples, i.seed), (3, 1000, SEED));
}

#[test]
fn the_interval_has_the_known_width_for_a_coin_flip_difference() {
    // 50 題:一半 B 比 A 好 1 分,一半一樣。平均差 0.5,標準誤 √(0.25/50) ≈ 0.0707,
    // 95% 區間約 0.5 ± 1.96 × 0.0707 = (0.361, 0.639)。分數是 1/50 的倍數,所以剛好是 (0.36, 0.64)。
    // 這組數字也用獨立的 Python 實作(同樣的 SplitMix64 與百分位定義)算過。
    // 取 90% 區間(第 5、95 百分位)會得到 (0.38, 0.62):這個測試分得出來。
    let pairs: Vec<(f64, f64)> = (0..50)
        .map(|i| (0.0, if i < 25 { 1.0 } else { 0.0 }))
        .collect();

    let i = paired_bootstrap(&pairs, RESAMPLES, SEED).unwrap();

    assert!(approx(i.diff, 0.5), "{i:?}");
    assert!(approx(i.low, 0.36) && approx(i.high, 0.64), "{i:?}");
    assert!(i.excludes_zero());
    assert!(!i.few_cases());
}

#[test]
fn the_same_seed_gives_the_same_interval_and_another_seed_does_not() {
    let pairs: Vec<(f64, f64)> = (0..40)
        .map(|i| (f64::from(i % 3) / 2.0, f64::from(i % 5) / 4.0))
        .collect();

    let a = paired_bootstrap(&pairs, 2000, 7).unwrap();
    let b = paired_bootstrap(&pairs, 2000, 7).unwrap();
    let c = paired_bootstrap(&pairs, 2000, 8).unwrap();

    assert_eq!(a, b);
    assert!(a.low != c.low || a.high != c.high, "{a:?} {c:?}");
    // 觀察到的差不看亂數。
    assert!(approx(a.diff, c.diff));
    assert!(a.low <= a.diff && a.diff <= a.high, "{a:?}");
}

#[test]
fn no_cases_or_no_resamples_give_no_interval() {
    assert_eq!(paired_bootstrap(&[], RESAMPLES, SEED), None);
    assert_eq!(paired_bootstrap(&[(0.0, 1.0)], 0, SEED), None);
}

fn interval(low: f64, high: f64, cases: usize) -> Interval {
    Interval {
        cases,
        diff: (low + high) / 2.0,
        low,
        high,
        resamples: RESAMPLES,
        seed: SEED,
    }
}

#[test]
fn an_interval_that_contains_zero_shows_no_difference() {
    assert!(interval(0.01, 0.2, 50).excludes_zero());
    assert!(interval(-0.2, -0.01, 50).excludes_zero());
    assert!(!interval(-0.1, 0.2, 50).excludes_zero());
    // 剛好碰到 0 也是「看不出差異」。
    assert!(!interval(0.0, 0.2, 50).excludes_zero());
    assert!(!interval(-0.2, 0.0, 50).excludes_zero());
}

#[test]
fn non_inferiority_looks_at_the_lower_bound() {
    // 小模型比大模型差,但沒有差超過 δ = 0.05。
    assert!(interval(-0.04, 0.02, 50).non_inferior(0.05));
    assert!(!interval(-0.06, 0.02, 50).non_inferior(0.05));
    // 剛好等於 −δ:不能說「沒有差超過」。
    assert!(!interval(-0.05, 0.02, 50).non_inferior(0.05));
}

#[test]
fn fewer_than_30_cases_is_flagged() {
    assert!(interval(0.1, 0.2, 29).few_cases());
    assert!(!interval(0.1, 0.2, 30).few_cases());
}

#[test]
fn the_interval_matches_an_independent_implementation_on_continuous_scores() {
    // 分數是連續值(無理數倍數的小數部分)時,百分位附近的值幾乎都不同:索引差一、或抽樣方式不同
    // (例如用 `%` 取餘數),結果就會不同。期待的數字由獨立的 Python 實作算出(同樣的 SplitMix64、Lemire 抽樣、
    // 加總順序與百分位定義);種子 1 是在 Python 裡確認過「第 2.5 與第 97.5 百分位兩側的值都沒有重複」的那個。
    // 這也把「同一個種子得到同一個區間」固定下來:改了抽樣方式,舊報告就重現不出來,這個測試會先失敗。
    // (手寫 mutation:上下界的索引差一、`below` 改成取餘數,在 0/1 分數的那個測試上都沒被抓到。)
    let fract = |x: f64| x - x.trunc();
    let pairs: Vec<(f64, f64)> = (0..40u32)
        .map(|i| {
            let i = f64::from(i);
            (fract(i * 0.618_033_988_7), fract(i * 0.414_213_562_3))
        })
        .collect();

    let i = paired_bootstrap(&pairs, RESAMPLES, 1).unwrap();

    assert_eq!(i.diff, 0.0005016851999994409);
    assert_eq!(i.low, -0.12179057064000057);
    assert_eq!(i.high, 0.12232121411999959);
    assert!(!i.excludes_zero());
}
