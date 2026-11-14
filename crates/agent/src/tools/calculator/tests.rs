use jiff::Timestamp;
use proptest::prelude::*;
use uuid::Uuid;

use super::*;
use crate::tool::{Clock, ErasedTool};

/// 一張表:每個算式的結果都要剛好等於預期的字串。
fn assert_results(cases: &[(&str, &str)]) {
    for (expression, expected) in cases {
        assert_eq!(
            evaluate(expression).as_deref(),
            Ok(*expected),
            "{expression:?}"
        );
    }
}

fn assert_errors(cases: &[(&str, &str)]) {
    for (expression, expected) in cases {
        let message = evaluate(expression).map_err(|e| e.to_string());
        assert_eq!(message, Err(expected.to_string()), "{expression:?}");
    }
}

#[test]
fn operators_follow_the_usual_precedence() {
    assert_results(&[
        ("1 + 2 * 3", "7"),
        ("(1 + 2) * 3", "9"),
        ("2 * 3 ^ 2", "18"),
        // 同一層由左往右算。
        ("10 - 4 - 3", "3"),
        ("100 / 10 / 5", "2"),
        // `^` 由右往左:2 ^ (3 ^ 2)。
        ("2 ^ 3 ^ 2", "512"),
        // 負號比 `^` 鬆:-(2 ^ 2),跟數學課本一樣。
        ("-2 ^ 2", "-4"),
        ("2 ^ -1", "0.5"),
        ("-(3 - 5)", "2"),
        // 負號比 `+` 緊:(-3) + 5,不是 -(3 + 5)。
        ("-3 + 5", "2"),
        ("--3", "3"),
        ("7 % 3", "1"),
        // 餘數的正負號跟被除數(Rust 與 Java 的 `%` 都是這樣)。
        ("-7 % 3", "-1"),
        ("8 - 2 * 3 + 1", "3"),
    ]);
}

#[test]
fn numbers_can_have_fractions_and_exponents() {
    assert_results(&[
        ("1.5e3 + 2E-1", "1500.2"),
        ("2e+2", "200"),
        (".5 + 1.", "1.5"),
        ("007", "7"),
    ]);
}

#[test]
fn functions_and_constants() {
    assert_results(&[
        ("sqrt(16)", "4"),
        ("abs(-3)", "3"),
        ("ln(e)", "1"),
        ("log10(1000)", "3"),
        ("floor(2.7)", "2"),
        ("ceil(2.1)", "3"),
        // 剛好一半時往離 0 遠的方向(Rust 的 `f64::round`)。
        ("round(2.5)", "3"),
        ("round(-2.5)", "-3"),
        ("pi", "3.141592653589793"),
        ("e", "2.718281828459045"),
        ("sqrt(9) * pi / pi + abs(-1)", "4"),
        ("sqrt((3 + 1) * 4)", "4"),
    ]);
}

#[test]
fn whitespace_is_ignored() {
    assert_results(&[(" 1+2 ", "3"), ("1\t+\n2", "3"), ("\u{3000}1 + 2", "3")]);
}

#[test]
fn results_are_written_for_people() {
    assert_results(&[
        // 2^53 以內的整數印出每一位。
        ("123456789 * 1000000", "123456789000000"),
        ("2 ^ 53", "9007199254740992"),
        ("9999999999999998", "9999999999999998"),
        // 從 1e16 起用科學記號(不然 1e300 會印出 301 位數)。
        ("10 ^ 16", "1e16"),
        ("2 ^ 60", "1.152921504606847e18"),
        ("1 / 3", "0.3333333333333333"),
        // 1e-6 以下用科學記號。
        ("0.000001", "0.000001"),
        ("0.000001 / 10", "1e-7"),
        // 沒有 -0。
        ("0 * -1", "0"),
        ("-0", "0"),
    ]);
}

#[test]
fn syntax_errors_say_what_and_where() {
    assert_errors(&[
        ("", "the expression is empty"),
        ("   ", "the expression is empty"),
        ("1 + # 2", "unexpected character '#' at position 5"),
        // 位置是第幾個字元,不是第幾個 byte(全形空白佔 3 bytes)。
        ("\u{3000}1 #", "unexpected character '#' at position 4"),
        ("π + 1", "unexpected character 'π' at position 1"),
        ("1.2.3", "invalid number \"1.2.3\" at position 1"),
        // 不支援省略乘號。
        ("2(3)", "unexpected \"(\" at position 2"),
        ("1 2", "unexpected \"2\" at position 3"),
        (")", "unexpected \")\" at position 1"),
        ("2 * )", "unexpected \")\" at position 5"),
        ("(1 + 2", "the expression ends too early"),
        // 括號裡讀完一個算式,下一個必須是 `)`。
        ("(1 2)", "unexpected \"2\" at position 4"),
        ("sqrt(4 5)", "unexpected \"5\" at position 8"),
        ("1 +", "the expression ends too early"),
        // `%` 是餘數,不是百分比。
        ("20%", "the expression ends too early"),
        ("2e", "unexpected \"e\" at position 2"),
        (
            "foo + 1",
            "unknown name \"foo\" at position 1; known names: pi, e, sqrt, abs, ln, log10, floor, ceil, round",
        ),
        (
            "PI",
            "unknown name \"PI\" at position 1; known names: pi, e, sqrt, abs, ln, log10, floor, ceil, round",
        ),
        ("sqrt 4", "\"sqrt\" needs parentheses, like sqrt(2)"),
        ("pi(2)", "unexpected \"(\" at position 3"),
    ]);
}

#[test]
fn results_that_are_not_finite_numbers_are_errors() {
    assert_errors(&[
        ("1 / 0", "division by zero"),
        ("5 % 0", "division by zero"),
        ("1 / (2 - 2)", "division by zero"),
        ("sqrt(-1)", "the result is not a finite number"),
        ("ln(0)", "the result is not a finite number"),
        ("10 ^ 400", "the result is not a finite number"),
        ("1e400", "the result is not a finite number"),
        // 中間的結果就要檢查:1 / ∞ = 0 是有限的數,但答案是錯的。
        ("1 / (10 ^ 400)", "the result is not a finite number"),
    ]);
}

#[test]
fn the_length_limit_is_exact() {
    // "11" + "+1" × 511 = 1024 bytes,剛好在上限。
    let longest = format!("11{}", "+1".repeat(511));
    assert_eq!(longest.len(), MAX_EXPRESSION_BYTES);
    assert_eq!(evaluate(&longest).as_deref(), Ok("522"));
    let too_long = format!("1{longest}");
    assert_eq!(evaluate(&too_long), Err(CalcError::TooLong));
}

#[test]
fn the_nesting_limit_is_exact() {
    // 最外層算一層,每個括號再一層:63 個括號是第 64 層。
    let deepest = format!("{}1{}", "(".repeat(63), ")".repeat(63));
    assert_eq!(evaluate(&deepest).as_deref(), Ok("1"));
    let too_deep = format!("({deepest})");
    assert_eq!(evaluate(&too_deep), Err(CalcError::TooDeep));
    // 負號、`^` 的右邊也是巢狀。
    assert_eq!(
        evaluate(&format!("{}1", "-".repeat(70))),
        Err(CalcError::TooDeep)
    );
    assert_eq!(
        evaluate(&format!("{}1", "1^".repeat(70))),
        Err(CalcError::TooDeep)
    );
}

#[test]
fn the_nesting_limit_holds_without_the_length_limit() {
    // 長度上限讓巢狀最多約 500 層,本來就不會 stack overflow;深度上限是另一道防線:
    // 之後有人放寬長度上限,遞迴的 parser 也不會因為 10 萬層括號讓整個程式中止。
    let huge = format!("{}1{}", "(".repeat(100_000), ")".repeat(100_000));
    assert_eq!(parse(&huge), Err(CalcError::TooDeep));
}

// ---- 當成工具用 ----

fn ctx() -> ToolContext {
    ToolContext {
        run_id: Uuid::nil(),
        call_id: "toolu_1".into(),
        clock: Clock::Fixed(Timestamp::UNIX_EPOCH),
    }
}

#[tokio::test]
async fn the_tool_answers_with_the_result() {
    assert_eq!(
        Calculator
            .call_json(r#"{"expression": "6 * 7"}"#, &ctx())
            .await,
        Ok("42".to_string())
    );
}

#[tokio::test]
async fn a_bad_expression_is_an_argument_error_and_bad_math_is_a_failure() {
    // 算式寫錯是參數的問題;算式沒錯但算不出來(除以 0)是工具執行失敗。模型兩種都能修正。
    assert_eq!(
        Calculator
            .call_json(r#"{"expression": "1 +"}"#, &ctx())
            .await,
        Err(ToolError::InvalidArgs(
            "the expression ends too early".into()
        ))
    );
    assert_eq!(
        Calculator
            .call_json(r#"{"expression": "1 / 0"}"#, &ctx())
            .await,
        Err(ToolError::Failed("division by zero".into()))
    );
}

#[test]
fn the_definition_sent_to_the_model_is_locked() {
    insta::assert_json_snapshot!(Calculator.def());
}

// ---- property tests ----

proptest! {
    /// 任意字串都只能回 Ok 或 Err,絕不 panic。
    #[test]
    fn never_panics_on_any_text(text in any::<String>()) {
        let _ = evaluate(&text);
    }

    /// 偏向算式的字元,才會走進 parser 的深處(純亂碼大多在 lexer 就被擋下)。
    #[test]
    fn never_panics_on_expression_like_text(text in "[0-9+*/%^().eE a-z-]{0,300}") {
        let _ = evaluate(&text);
    }

    /// 整數的加減乘跟 i64 的答案一樣(範圍內的結果都小於 2^53,f64 表示得剛好)。
    #[test]
    fn integer_arithmetic_is_exact(a in -1_000_000i64..1_000_000, b in -1_000_000i64..1_000_000) {
        prop_assert_eq!(evaluate(&format!("{a} + {b}")), Ok((a + b).to_string()));
        prop_assert_eq!(evaluate(&format!("{a} - {b}")), Ok((a - b).to_string()));
        prop_assert_eq!(evaluate(&format!("{a} * {b}")), Ok((a * b).to_string()));
        prop_assert_eq!(evaluate(&format!("({a}) * ({b}) - {a}")), Ok((a * b - a).to_string()));
    }
}
