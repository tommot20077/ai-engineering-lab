//! 確定性檢查:程式自己就能判斷,不呼叫 API、不花錢,每次結果都一樣(D-031)。
//!
//! `compiles`(003 第三部分)要在沙箱裡編譯回答裡的程式碼:要等、而且需要 `Sandbox`。其他檢查都是馬上算完。

use std::sync::Arc;

use futures::future::BoxFuture;
use sandbox::{Action, Sandbox};
use serde_json::Value;

use super::{Scored, Scorer, Verdict, VerdictResult};
use crate::case::{Case, Check, Output, regex_problem};

/// 打分結果裡 `scorer` 欄位的名字。
pub const NAME: &str = "check";

/// 依案例的 `checks` 逐條檢查。
///
/// `sandbox`:`compiles` 用的。沒有的話 `compiles` 記成錯誤(判斷不了),不是沒通過。
// `Option<Arc<dyn Sandbox>>`:可能沒有(只做文字檢查的資料集不需要 docker);有的話是共用的 trait object,
// 同一次打分的每一題都用同一個(容器是每次編譯各開一個,沙箱物件本身沒有狀態)。
#[derive(Clone, Default)]
pub struct Checks {
    sandbox: Option<Arc<dyn Sandbox>>,
}

impl Checks {
    pub fn new(sandbox: Option<Arc<dyn Sandbox>>) -> Checks {
        Checks { sandbox }
    }

    /// 沒有沙箱(`compiles` 會記成錯誤)。`const fn`:可以放進 `static`,測試裡借出 `&'static Checks`。
    pub const fn offline() -> Checks {
        Checks { sandbox: None }
    }

    /// `compiles` 用來擷取程式碼與編譯;其他檢查同步算完。
    async fn outcome(&self, check: &Check, answer: &str) -> Outcome {
        match check {
            Check::Compiles {} => self.compiles(answer).await,
            other => evaluate(other, answer),
        }
    }

    async fn compiles(&self, answer: &str) -> Outcome {
        let Some(code) = last_rust_block(answer) else {
            return Outcome::Fail("the answer has no ```rust code block".into());
        };
        let Some(sandbox) = &self.sandbox else {
            return Outcome::Broken(
                "compiles needs the sandbox (see [sandbox] in eval.toml)".into(),
            );
        };
        match sandbox.rust(code, Action::Compile).await {
            Ok(report) if report.compile.success() => Outcome::Pass,
            Ok(report) if report.compile.timed_out => {
                Outcome::Fail("the code did not compile within the time limit".into())
            }
            // 理由只放 rustc 的第一行錯誤(`error[E0308]: mismatched types`),不放程式碼本身:回答是私有內容。
            Ok(report) => Outcome::Fail(format!(
                "the code does not compile: {}",
                first_error(&report.compile.stderr)
            )),
            // docker 沒開、image 不在:不是模型的錯,記成錯誤(之後可以重打分)。
            Err(e) => Outcome::Broken(format!("the sandbox failed: {e}")),
        }
    }
}

impl Scorer for Checks {
    fn name(&self) -> &str {
        NAME
    }

    fn score<'a>(&'a self, case: &'a Case, output: &'a Output) -> BoxFuture<'a, Scored> {
        Box::pin(async move {
            let answer = output.text();
            let mut verdicts = Vec::with_capacity(case.checks.len());
            // 一條一條照順序檢查(結果的順序 = 檢查的順序);只有 `compiles` 真的要等。
            for check in &case.checks {
                verdicts.push(verdict(check, self.outcome(check, &answer).await));
            }
            Scored {
                verdicts,
                ..Scored::default()
            }
        })
    }
}

/// 回答裡最後一個 ` ```rust ` 區塊的內容(不含圍欄)。圍欄的語言標記第一個字是 `rust` 就算(`rust,ignore` 也算);
/// 沒有收尾的區塊不算。取最後一個:回答常常先引用題目的程式,最後才給修好的版本。
pub(crate) fn last_rust_block(text: &str) -> Option<&str> {
    let mut found = None;
    let mut open: Option<usize> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let trimmed = line.trim();
        match open {
            None => {
                if let Some(info) = trimmed.strip_prefix("```") {
                    let lang = info.split([',', ' ', '\t']).next().unwrap_or_default();
                    // 其他語言的區塊也要配對到收尾,裡面的 ```rust 字樣不算。
                    open = Some(if lang == "rust" { offset } else { usize::MAX });
                }
            }
            Some(body) if trimmed.chars().all(|c| c == '`') && trimmed.len() >= 3 => {
                if body != usize::MAX {
                    found = text.get(body..start);
                }
                open = None;
            }
            Some(_) => {}
        }
    }
    found
}

/// rustc 輸出的第一行 `error` 開頭的訊息,最多 200 個字元。
fn first_error(stderr: &str) -> String {
    let line = stderr
        .lines()
        .find(|l| l.starts_with("error"))
        .unwrap_or("rustc reported an error");
    line.chars().take(200).collect()
}

fn verdict(check: &Check, outcome: Outcome) -> Verdict {
    let (result, reason) = match outcome {
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
        // 要沙箱、要等:在 `Checks::outcome` 處理,不會走到這裡。
        Check::Compiles {} => Outcome::Broken("compiles is checked by Checks".into()),
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
            block_on(Checks::default().score(&case(vec![check]), &output(vec![text(answer)])))
                .verdicts;
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
        let scored = block_on(Checks::default().score(&case(checks), &output(vec![text("abc")])));
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
        let scorers: Vec<Box<dyn Scorer>> = vec![Box::new(Checks::default())];
        let names: Vec<&str> = scorers.iter().map(|s| s.name()).collect();
        assert_eq!(names, ["check"]);
        assert!(
            block_on(scorers[0].score(&case(vec![]), &output(vec![])))
                .verdicts
                .is_empty()
        );
    }

    // ---- compiles(003 第三部分)----

    use std::sync::Mutex;

    use sandbox::{Output as Ran, Report, SandboxError};

    /// 假的沙箱:記下收到的程式與動作,回傳事先給的編譯結果。
    struct FakeSandbox {
        reply: Result<Ran, SandboxError>,
        received: Mutex<Vec<(String, Action)>>,
    }

    impl Sandbox for FakeSandbox {
        fn rust<'a>(
            &'a self,
            source: &'a str,
            action: Action,
        ) -> BoxFuture<'a, Result<Report, SandboxError>> {
            self.received
                .lock()
                .unwrap()
                .push((source.to_owned(), action));
            let reply = self
                .reply
                .clone()
                .map(|compile| Report { compile, run: None });
            Box::pin(async move { reply })
        }
    }

    fn ran(exit_code: Option<i32>, stderr: &str, timed_out: bool) -> Ran {
        Ran {
            exit_code,
            stdout: String::new(),
            stderr: stderr.into(),
            timed_out,
            truncated: false,
        }
    }

    /// 判決(結果、理由)與沙箱收到的(程式、動作)。
    type Judged = ((VerdictResult, Option<String>), Vec<(String, Action)>);

    fn compiles_with(reply: Result<Ran, SandboxError>, answer: &str) -> Judged {
        let fake = Arc::new(FakeSandbox {
            reply,
            received: Mutex::new(Vec::new()),
        });
        let checks = Checks::new(Some(fake.clone()));
        let verdicts =
            block_on(checks.score(&case(vec![Check::Compiles {}]), &output(vec![text(answer)])))
                .verdicts;
        let v = verdicts.into_iter().next().unwrap();
        assert_eq!(
            (v.scorer.as_str(), v.criterion.as_str()),
            ("check", "compiles")
        );
        let received = fake.received.lock().unwrap().clone();
        ((v.result, v.reason), received)
    }

    const ANSWER: &str = "改成這樣:\n\n```rust\nfn main() {}\n```\n";

    #[test]
    fn the_last_rust_block_is_compiled_and_only_compiled() {
        let (verdict, received) = compiles_with(Ok(ran(Some(0), "", false)), ANSWER);
        assert_eq!(verdict, (Pass, None));
        assert_eq!(received, [("fn main() {}\n".to_owned(), Action::Compile)]);
    }

    #[test]
    fn a_compile_error_fails_with_rustc_first_error_line_only() {
        let stderr = "warning: unused\nerror[E0308]: mismatched types\n --> main.rs:1:20\n  |\n1 | let x: u8 = \"secret\";\n";
        let (verdict, _) = compiles_with(Ok(ran(Some(1), stderr, false)), ANSWER);
        assert_eq!(
            verdict,
            (
                Fail,
                Some("the code does not compile: error[E0308]: mismatched types".into())
            )
        );
        let (verdict, _) = compiles_with(Ok(ran(Some(1), "", false)), ANSWER);
        assert_eq!(
            verdict,
            (
                Fail,
                Some("the code does not compile: rustc reported an error".into())
            )
        );
        let (verdict, _) = compiles_with(Ok(ran(None, "", true)), ANSWER);
        assert_eq!(
            verdict,
            (
                Fail,
                Some("the code did not compile within the time limit".into())
            )
        );
    }

    #[test]
    fn an_answer_without_a_rust_block_fails_without_touching_the_sandbox() {
        let (verdict, received) = compiles_with(
            Ok(ran(Some(0), "", false)),
            "fn main() {} // 沒有程式碼區塊",
        );
        assert_eq!(
            verdict,
            (Fail, Some("the answer has no ```rust code block".into()))
        );
        assert!(received.is_empty());
    }

    #[test]
    fn a_missing_or_broken_sandbox_is_an_error_not_a_fail() {
        let (verdict, _) = compiles_with(
            Err(SandboxError::Unavailable {
                program: "docker".into(),
                reason: "not found".into(),
            }),
            ANSWER,
        );
        assert_eq!(
            verdict,
            (
                Error,
                Some(
                    "the sandbox failed: could not run `docker` (is it installed?): not found"
                        .into()
                )
            )
        );
        let offline = block_on(
            Checks::offline().score(&case(vec![Check::Compiles {}]), &output(vec![text(ANSWER)])),
        );
        assert_eq!(offline.verdicts[0].result, Error);
        assert_eq!(
            offline.verdicts[0].reason.as_deref(),
            Some("compiles needs the sandbox (see [sandbox] in eval.toml)")
        );
    }

    #[test]
    fn rust_blocks_are_found_by_their_fence() {
        let cases: [(&str, Option<&str>); 9] = [
            ("```rust\na\n```", Some("a\n")),
            // 取最後一個(先引用題目、最後給答案)。
            (
                "```rust\nold\n```\n說明\n```rust\nnew\n```\n",
                Some("new\n"),
            ),
            ("```rust,ignore\nx\n```", Some("x\n")),
            ("```rust no_run\nx\n```", Some("x\n")),
            ("  ```rust\n  x\n  ```", Some("  x\n")),
            ("```rust\r\nx\r\n```\r\n", Some("x\r\n")),
            // 不是 rust、或沒有收尾:不算。
            ("```text\nfn main() {}\n```", None),
            ("```rust\nfn main() {}\n", None),
            // 其他語言區塊裡的 ```rust 字樣不算開頭。
            ("````md\n```rust\nx\n```\n````", None),
        ];
        for (text, expected) in cases {
            assert_eq!(last_rust_block(text), expected, "{text:?}");
        }
        assert_eq!(last_rust_block("```rustc\nx\n```"), None);
        assert_eq!(last_rust_block("```rust\n```"), Some(""));
    }

    #[test]
    fn compiles_is_written_as_a_bare_type() {
        let check: Check = serde_json::from_value(json!({"type": "compiles"})).unwrap();
        assert_eq!(check, Check::Compiles {});
        assert_eq!(
            serde_json::to_value(&check).unwrap(),
            json!({"type": "compiles"})
        );
        assert_eq!(check.to_string(), "compiles");
        assert!(serde_json::from_value::<Check>(json!({"type": "compiles", "x": 1})).is_err());
    }
}
