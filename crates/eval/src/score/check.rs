//! 確定性檢查:程式自己就能判斷,不呼叫 API、不花錢,每次結果都一樣(D-031)。

use futures::future::{self, BoxFuture};
use serde_json::Value;

use super::{Scored, Scorer, Verdict, VerdictResult};
use crate::case::{Case, Check, Output, regex_problem};

const NAME: &str = "check";

/// 依案例的 `checks` 逐條檢查。
pub struct Checks;

impl Scorer for Checks {
    fn name(&self) -> &str {
        NAME
    }

    fn score<'a>(&'a self, case: &'a Case, output: &'a Output) -> BoxFuture<'a, Scored> {
        let answer = output.text();
        let verdicts = case
            .checks
            .iter()
            .map(|check| verdict(check, &answer))
            .collect();
        // 確定性檢查不需要等任何東西:`future::ready` 是「已經完成」的 future。
        // 為了跟評審用同一個介面,還是包成 future。Java 對照:`CompletableFuture.completedFuture(...)`。
        Box::pin(future::ready(Scored {
            verdicts,
            ..Scored::default()
        }))
    }
}

fn verdict(check: &Check, answer: &str) -> Verdict {
    let (result, reason) = match evaluate(check, answer) {
        Outcome::Pass => (VerdictResult::Pass, None),
        Outcome::Fail(reason) => (VerdictResult::Fail, Some(reason)),
        Outcome::Broken(reason) => (VerdictResult::Error, Some(reason)),
    };
    Verdict {
        scorer: NAME.into(),
        criterion: check.to_string(),
        result,
        reason,
    }
}

enum Outcome {
    Pass,
    /// 回答不符合。理由不引用回答本身(結果檔可能被貼給別人看,回答是私有內容)。
    Fail(String),
    /// 檢查本身壞了,判斷不了。
    Broken(String),
}

fn evaluate(check: &Check, answer: &str) -> Outcome {
    match check {
        Check::Exact { value } => pass_if(answer.trim() == value.trim(), || {
            format!("the answer is not exactly {value:?}")
        }),
        Check::Contains { value } => pass_if(answer.contains(value.as_str()), || {
            format!("{value:?} not found")
        }),
        Check::NotContains { value } => pass_if(!answer.contains(value.as_str()), || {
            format!("{value:?} found")
        }),
        Check::Regex { pattern } => match regex::Regex::new(pattern) {
            Ok(re) => pass_if(re.is_match(answer), || "no match".into()),
            Err(e) => Outcome::Broken(format!("invalid regex: {}", regex_problem(&e))),
        },
        Check::Json { required } => json(answer, required),
    }
}

/// 條件成立就通過;不成立才呼叫 `reason` 組出理由(通過時不必白白組字串)。
// `impl FnOnce() -> String`:參數是「任何一個呼叫一次、回傳 String 的 closure」,每種 closure 各編譯一份,沒有額外成本。
// Java 對照:`Supplier<String>`,像 `Objects.requireNonNull(x, () -> "message")` 那種延後產生訊息的寫法。
fn pass_if(ok: bool, reason: impl FnOnce() -> String) -> Outcome {
    if ok {
        Outcome::Pass
    } else {
        Outcome::Fail(reason())
    }
}

/// 整個回答(去掉前後空白)必須是 JSON;有 `required` 時,最上層必須是物件而且有這些欄位。
fn json(answer: &str, required: &[String]) -> Outcome {
    let value: Value = match serde_json::from_str(answer.trim()) {
        Ok(value) => value,
        Err(e) => return Outcome::Fail(format!("not valid JSON: {e}")),
    };
    if required.is_empty() {
        return Outcome::Pass;
    }
    let Value::Object(fields) = value else {
        return Outcome::Fail("the top level is not an object".into());
    };
    let missing: Vec<&str> = required
        .iter()
        .filter(|name| !fields.contains_key(name.as_str()))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail(format!("missing fields: {}", missing.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use llm::{Content, ProviderMetadata, Response, StopKind, StopReason, Usage};
    use serde_json::json;

    use super::*;
    use crate::case::{ChatInput, Check, Input};
    use crate::score::VerdictResult::{self, Error, Fail, Pass};

    fn text(s: &str) -> Content {
        Content::Text {
            text: s.into(),
            meta: ProviderMetadata::default(),
        }
    }

    fn output(content: Vec<Content>) -> Output {
        Output::Chat(Response {
            id: None,
            model: "m".into(),
            content,
            stop: StopReason {
                kind: StopKind::EndTurn,
                raw: "end_turn".into(),
            },
            usage: Usage::default(),
            meta: ProviderMetadata::default(),
        })
    }

    fn case(checks: Vec<Check>) -> Case {
        Case {
            id: "a".into(),
            tags: vec![],
            input: Input::Chat(ChatInput {
                system: None,
                messages: vec![],
                tools: vec![],
                max_tokens: 64,
                provider_options: ProviderMetadata::default(),
            }),
            checks,
            rubric: vec![],
            reference: None,
        }
    }

    /// 只有一條檢查時的判決。
    fn judge(check: Check, answer: &str) -> (VerdictResult, Option<String>) {
        let verdicts =
            block_on(Checks.score(&case(vec![check]), &output(vec![text(answer)]))).verdicts;
        assert_eq!(verdicts.len(), 1, "{verdicts:?}");
        let v = verdicts.into_iter().next().unwrap();
        (v.result, v.reason)
    }

    fn value(v: &str) -> String {
        v.to_owned()
    }

    #[test]
    fn each_check_passes_or_fails_with_a_reason() {
        let pass = (Pass, None);
        let fail = |reason: &str| (Fail, Some(reason.to_owned()));
        let cases = [
            // exact:前後空白不算
            (Check::Exact { value: value("42") }, "  42\n", pass.clone()),
            (
                Check::Exact { value: value("42") },
                "42.",
                fail(r#"the answer is not exactly "42""#),
            ),
            // contains / not_contains:分大小寫
            (
                Check::Contains {
                    value: value("所有權"),
                },
                "Rust 的所有權規則",
                pass.clone(),
            ),
            (
                Check::Contains {
                    value: value("Move"),
                },
                "move semantics",
                fail(r#""Move" not found"#),
            ),
            (
                Check::NotContains {
                    value: value("unsafe"),
                },
                "safe code",
                pass.clone(),
            ),
            (
                Check::NotContains {
                    value: value("unsafe"),
                },
                "uses unsafe",
                fail(r#""unsafe" found"#),
            ),
            // regex:有一處符合就算
            (
                Check::Regex {
                    pattern: value(r"mo+ve"),
                },
                "it moooves",
                pass.clone(),
            ),
            (
                Check::Regex {
                    pattern: value(r"^\d+$"),
                },
                "12a",
                fail("no match"),
            ),
        ];
        for (check, answer, expected) in cases {
            let label = check.to_string();
            assert_eq!(judge(check, answer), expected, "{label} on {answer:?}");
        }
    }

    #[test]
    fn json_checks_parse_the_whole_answer() {
        let json = |required: &[&str]| Check::Json {
            required: required.iter().map(|s| (*s).to_owned()).collect(),
        };
        assert_eq!(
            judge(json(&["a", "b"]), r#" {"a": 1, "b": null} "#),
            (Pass, None)
        );
        assert_eq!(judge(json(&[]), "[1, 2]"), (Pass, None));
        assert_eq!(
            judge(json(&["a", "b", "c"]), r#"{"b": 1}"#),
            (Fail, Some("missing fields: a, c".into()))
        );
        assert_eq!(
            judge(json(&["a"]), "[1]"),
            (Fail, Some("the top level is not an object".into()))
        );
        // 包在 ``` 裡就不是 JSON:要 JSON 的程式拿到這種回答一樣會解析失敗,所以算不通過。
        let (result, reason) = judge(json(&[]), "```json\n{}\n```");
        assert_eq!(result, Fail);
        assert!(reason.unwrap().starts_with("not valid JSON: "));
    }

    #[test]
    fn criterion_describes_the_check() {
        let described = [
            Check::Exact { value: value("42") },
            Check::Contains {
                value: value("所有權"),
            },
            Check::NotContains {
                value: value("a\"b"),
            },
            Check::Regex {
                pattern: value(r"^\d+$"),
            },
            Check::Json { required: vec![] },
            Check::Json {
                required: vec![value("a"), value("b")],
            },
        ]
        .map(|c| c.to_string());
        assert_eq!(
            described,
            [
                r#"exact "42""#,
                r#"contains "所有權""#,
                r#"not_contains "a\"b""#,
                r#"regex /^\d+$/"#,
                "json",
                "json with fields a, b",
            ]
        );
    }

    #[test]
    fn verdicts_follow_the_order_of_the_checks() {
        let checks = vec![
            Check::Contains { value: value("a") },
            Check::Contains { value: value("z") },
        ];
        let scored = block_on(Checks.score(&case(checks), &output(vec![text("abc")])));
        // 確定性檢查不花錢:用量是 0。
        assert_eq!(scored.usage, llm::Usage::default());
        let verdicts = scored.verdicts;
        assert_eq!(
            verdicts,
            [
                Verdict {
                    scorer: "check".into(),
                    criterion: r#"contains "a""#.into(),
                    result: Pass,
                    reason: None,
                },
                Verdict {
                    scorer: "check".into(),
                    criterion: r#"contains "z""#.into(),
                    result: Fail,
                    reason: Some(r#""z" not found"#.into()),
                },
            ]
        );
    }

    #[test]
    fn a_broken_regex_is_an_error_not_a_fail() {
        // 讀資料集時就會擋下壞掉的 regex;這裡防的是「沒經過資料集就直接建出來的案例」。
        // 打分器自己壞了,不能說成「回答不通過」(fail-closed:也不能說成通過)。
        let (result, reason) = judge(
            Check::Regex {
                pattern: value("("),
            },
            "anything",
        );
        assert_eq!(result, Error);
        assert_eq!(reason.as_deref(), Some("invalid regex: unclosed group"));
    }

    #[test]
    fn only_text_blocks_count_as_the_answer() {
        let out = output(vec![
            Content::Thinking {
                text: "THINKING".into(),
                meta: ProviderMetadata::default(),
            },
            text("first"),
            Content::ToolCall {
                id: "t".into(),
                name: "read".into(),
                args_json: "{}".into(),
                meta: ProviderMetadata::default(),
            },
            text("second"),
        ]);
        assert_eq!(out.text(), "first\nsecond");
    }

    #[test]
    fn verdicts_serialize_to_the_scores_file_format() {
        // 第 5 步寫進 `scores/<name>.jsonl` 的就是這個樣子;沒有理由時不寫 `reason`。
        let pass = Verdict {
            scorer: "check".into(),
            criterion: "json".into(),
            result: Pass,
            reason: None,
        };
        assert_eq!(
            serde_json::to_value(&pass).unwrap(),
            json!({ "scorer": "check", "criterion": "json", "result": "pass" })
        );
    }

    #[test]
    fn scorers_of_different_kinds_fit_in_one_list() {
        // 這個測試主要是「能編譯」:`dyn Scorer` 放得進 Vec,第 4 步的評審也會放進來。
        let scorers: Vec<Box<dyn Scorer>> = vec![Box::new(Checks)];
        let names: Vec<&str> = scorers.iter().map(|s| s.name()).collect();
        assert_eq!(names, ["check"]);
        assert!(
            block_on(scorers[0].score(&case(vec![]), &output(vec![])))
                .verdicts
                .is_empty()
        );
    }
}
