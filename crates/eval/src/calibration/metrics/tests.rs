use super::*;

use Expected::{Fail as LabelFail, Pass as LabelPass};
use VerdictResult::{Error, Fail, Pass, Skipped};

/// 期待的數字由獨立的 Python 實作算出:z 用 `statistics.NormalDist().inv_cdf(0.975)` 現場算,
/// 不抄這裡的常數;公式照 Wilson(1927)寫,沒有夾在 [0, 1](92/92 的上界因此是 1.0000000000000002)。
const KNOWN: [(usize, usize, f64, f64); 10] = [
    (27, 28, 0.8228780225664669, 0.9936674801509624),
    (25, 28, 0.7280414845132785, 0.9628816331707),
    (20, 28, 0.5294071156670429, 0.847460039433309),
    (1, 92, 0.001921329040091714, 0.059027856130625174),
    (0, 92, 0.0, 0.04008138928562171),
    (92, 92, 0.9599186107143785, 1.0),
    (5, 10, 0.23659309051256405, 0.763406909487436),
    (1, 1, 0.20654931437723745, 1.0),
    (0, 1, 0.0, 0.7934506856227626),
    (3, 7, 0.1582198552514697, 0.7495416354723428),
];

#[test]
fn wilson_matches_an_independent_implementation() {
    for (k, n, low, high) in KNOWN {
        let p = Proportion::new(k, n).unwrap();
        assert_eq!((p.k, p.n), (k, n));
        assert!(
            (p.rate - k as f64 / n as f64).abs() < 1e-15,
            "{k}/{n}: rate {}",
            p.rate
        );
        assert!(
            (p.low - low).abs() < 1e-12,
            "{k}/{n}: low {} vs {low}",
            p.low
        );
        assert!(
            (p.high - high).abs() < 1e-12,
            "{k}/{n}: high {} vs {high}",
            p.high
        );
    }
}

#[test]
fn the_interval_stays_between_0_and_1() {
    // 浮點誤差會讓 92/92 的上界算成 1.0000000000000002、0/5 的下界算成 2.8e-17(比 0 還大)。
    assert_eq!(Proportion::new(92, 92).unwrap().high, 1.0);
    assert_eq!(Proportion::new(0, 5).unwrap().low, 0.0);
    for n in 1..200 {
        for k in [0, 1, n / 2, n - 1, n] {
            let p = Proportion::new(k, n).unwrap();
            assert!(
                0.0 <= p.low && p.low <= p.rate && p.rate <= p.high && p.high <= 1.0,
                "{k}/{n}: {p:?}"
            );
        }
    }
}

#[test]
fn no_labels_is_no_evidence_not_zero() {
    assert_eq!(Proportion::new(0, 0), None);
    let none: Tally = [(LabelPass, Pass)].into_iter().collect();
    assert_eq!(
        none.catch_rate(),
        None,
        "沒有標成 fail 的標準,抓錯率不是 0% 也不是 100%"
    );
    let none: Tally = [(LabelFail, Fail)].into_iter().collect();
    assert_eq!(none.false_fail_rate(), None);
}

#[test]
fn a_tally_counts_every_combination() {
    // 每一種(標籤, 判決)的組合都出現,而且次數各不相同:數錯任何一格都看得出來。
    let mut pairs = Vec::new();
    for (label, verdict, times) in [
        (LabelFail, Fail, 1),
        (LabelFail, Pass, 2),
        (LabelFail, Error, 3),
        (LabelFail, Skipped, 4),
        (LabelPass, Pass, 5),
        (LabelPass, Fail, 6),
        (LabelPass, Error, 7),
        (LabelPass, Skipped, 8),
    ] {
        pairs.extend(std::iter::repeat_n((label, verdict), times));
    }

    let tally: Tally = pairs.into_iter().collect();

    assert_eq!(
        tally,
        Tally {
            expected_fail: 1 + 2 + 3 + 4,
            caught: 1,
            passed_through: 2,
            expected_pass: 5 + 6 + 7 + 8,
            false_fail: 6,
            errors: 3 + 4 + 7 + 8,
        }
    );
}

#[test]
fn each_rate_has_its_own_denominator_and_an_error_is_not_a_catch() {
    let tally = Tally {
        expected_fail: 4,
        caught: 3,
        passed_through: 0,
        expected_pass: 10,
        false_fail: 1,
        errors: 1, // 標成 fail 的那條評審出錯:不算抓到,但仍然在分母裡(fail-closed)
    };
    // 直接比 (k, n),不拿 `Proportion::new` 當預期值:兩邊都是 `None` 時也會「相等」。
    let caught = tally.catch_rate().unwrap();
    assert_eq!((caught.k, caught.n, caught.rate), (3, 4, 0.75));
    let false_fail = tally.false_fail_rate().unwrap();
    assert_eq!((false_fail.k, false_fail.n, false_fail.rate), (1, 10, 0.1));
}

#[test]
fn agreement_counts_only_criteria_both_judges_decided() {
    // 一致 3、不一致 2:數字不對稱,「把相同當成不同」的實作才會得到不一樣的結果。
    let pairs = [
        (Pass, Pass),
        (Pass, Pass),
        (Fail, Fail),
        (Pass, Fail),
        (Fail, Pass),
        (Error, Pass),
        (Fail, Skipped),
        (Error, Error),
    ];
    let p = agreement(pairs).unwrap();
    assert_eq!((p.k, p.n, p.rate), (3, 5, 0.6));
    assert_eq!(agreement([(Error, Pass), (Skipped, Skipped)]), None);
}
