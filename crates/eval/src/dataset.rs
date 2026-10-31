//! 資料集:一個 JSONL 檔,一行一個案例(design §4、D-030)。
//!
//! 讀檔時就檢查全部的案例,有問題就拒絕整個資料集(寫入路徑 fail-loud):
//! 跑到一半才發現第 37 題寫錯,前面花的 API 費用就浪費了。

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::case::Case;

/// 讀進來、檢查過的資料集。
#[derive(Debug)]
pub struct Dataset {
    /// 檔名去掉副檔名,例如 `rust-basics`。
    pub name: String,
    /// 檔案內容的 SHA-256(十六進位)。記進每次執行的 `meta.json`,比較時就知道資料集有沒有被改過。
    pub sha256: String,
    pub cases: Vec<Case>,
}

#[derive(Debug, thiserror::Error)]
pub enum DatasetError {
    #[error("cannot read dataset {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// 一次列出**全部**的問題,每一條都帶行號。
    #[error("invalid dataset `{name}`:\n{}", bullet_list(problems))]
    Invalid { name: String, problems: Vec<String> },
}

fn bullet_list(problems: &[String]) -> String {
    problems
        .iter()
        .map(|p| format!("  - {p}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Dataset {
    /// 讀一個資料集檔案。名稱是檔名去掉副檔名。
    pub fn load(path: &Path) -> Result<Self, DatasetError> {
        let bytes = std::fs::read(path).map_err(|source| DatasetError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        // `file_stem`:`evals/rust-basics.jsonl` → `rust-basics`。
        // `to_string_lossy`:檔名不一定是合法的 UTF-8(Unix 允許任何位元組),不合法的部分換成 �。
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self::parse(&name, &bytes)
    }

    /// 解析資料集的內容。純函式:不碰檔案系統,測試直接餵字串。
    pub fn parse(name: &str, bytes: &[u8]) -> Result<Self, DatasetError> {
        let invalid = |problems| DatasetError::Invalid {
            name: name.to_owned(),
            problems,
        };
        let text = std::str::from_utf8(bytes).map_err(|e| {
            // `valid_up_to`:前面這麼多 bytes 是合法的。數一數裡面有幾個換行,就知道壞在第幾行。
            let line = line_of(bytes, e.valid_up_to());
            invalid(vec![format!("line {line}: not valid UTF-8")])
        })?;

        let mut problems = Vec::new();
        let mut cases = Vec::new();
        // id → 第一次出現的行號。Java 對照:`Map<String, Integer>`。
        let mut seen: HashMap<String, usize> = HashMap::new();
        // `lines()` 同時處理 `\n` 與 `\r\n`;`enumerate()` 從 0 開始,行號要 +1。
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            if raw.trim().is_empty() {
                continue;
            }
            let case: Case = match serde_json::from_str(raw) {
                Ok(case) => case,
                Err(e) => {
                    problems.push(format!(
                        "line {line}, column {}: {}",
                        e.column(),
                        without_position(&e)
                    ));
                    continue;
                }
            };
            // 空的 id 不放進 `seen`:它已經被報一次了,不需要再報「重複」。
            if !case.id.is_empty() {
                // `entry` API:查一次就決定「已經有了」或「放進去」,不必先 get 再 insert(查兩次)。
                // Java 對照:`map.putIfAbsent(id, line)`,回傳值不是 null 就代表已經有了。
                match seen.entry(case.id.clone()) {
                    Entry::Occupied(first) => problems.push(format!(
                        "line {line}: duplicate id `{}` (first used on line {})",
                        case.id,
                        first.get()
                    )),
                    Entry::Vacant(slot) => {
                        slot.insert(line);
                    }
                }
            }
            for problem in case.problems() {
                problems.push(match case.id.as_str() {
                    "" => format!("line {line}: {problem}"),
                    id => format!("line {line} (`{id}`): {problem}"),
                });
            }
            cases.push(case);
        }
        if cases.is_empty() && problems.is_empty() {
            problems.push("the dataset has no cases".into());
        }
        if !problems.is_empty() {
            return Err(invalid(problems));
        }
        Ok(Self {
            name: name.to_owned(),
            sha256: sha256_hex(bytes),
            cases,
        })
    }
}

/// 第 `offset` 個 byte 在第幾行(從 1 開始)。
fn line_of(bytes: &[u8], offset: usize) -> usize {
    let before = bytes.get(..offset).unwrap_or(bytes);
    before.iter().filter(|&&b| b == b'\n').count() + 1
}

/// serde_json 的錯誤訊息最後會接「at line 1 column 23」。每一行是分開解析的,那個 line 永遠是 1,
/// 會誤導人;行號由我們自己加,這裡只留下前面的說明。
fn without_position(e: &serde_json::Error) -> String {
    let message = e.to_string();
    match message.rsplit_once(" at line ") {
        Some((head, _)) => head.to_owned(),
        None => message,
    }
}

/// SHA-256,寫成 64 個十六進位字元。
fn sha256_hex(bytes: &[u8]) -> String {
    // `Sha256::digest` 來自 `Digest` trait:trait 要 `use` 進來,它的方法才叫得到。
    // Java 對照:`MessageDigest.getInstance("SHA-256").digest(bytes)`,再自己轉成十六進位。
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use llm::{Content, Role};

    use super::*;
    use crate::case::{Check, Input};

    /// 一行合法的案例;`fields` 會接在 `id` 後面,用來改寫或加上欄位。
    fn line(id: &str, fields: &str) -> String {
        format!(
            r#"{{"id":"{id}","input":{{"type":"chat","max_tokens":64,"messages":[{{"role":"user","content":[{{"type":"text","text":"hi"}}]}}]}}{fields}}}"#
        )
    }

    const CHECK: &str = r#","checks":[{"type":"contains","value":"hello"}]"#;

    fn problems(text: &str) -> Vec<String> {
        match Dataset::parse("ds", text.as_bytes()) {
            Err(DatasetError::Invalid { problems, .. }) => problems,
            other => panic!("應該被拒絕:{other:?}"),
        }
    }

    #[test]
    fn parses_every_case_and_skips_blank_lines() {
        let text = format!(
            "{}\n\n{}\n",
            line("a", CHECK),
            line(
                "b",
                r#","tags":["rust"],"checks":[{"type":"regex","pattern":"mo+ve"}],"rubric":["說明 move"],"reference":"參考答案""#
            )
        );

        let ds = Dataset::parse("rust-basics", text.as_bytes()).unwrap();

        assert_eq!(ds.name, "rust-basics");
        let ids: Vec<&str> = ds.cases.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        let a = &ds.cases[0];
        assert_eq!(
            a.checks,
            [Check::Contains {
                value: "hello".into()
            }]
        );
        let Input::Chat(input) = &a.input;
        assert_eq!(input.max_tokens, 64);
        assert_eq!(input.messages[0].role, Role::User);
        let b = &ds.cases[1];
        assert_eq!(b.tags, ["rust"]);
        // 合法的 regex 檢查要能通過檢查(cargo-mutants:「pattern 是空的」換成永遠成立時,原本沒有測試失敗)。
        assert_eq!(
            b.checks,
            [Check::Regex {
                pattern: "mo+ve".into()
            }]
        );
        assert_eq!(b.rubric, ["說明 move"]);
        assert_eq!(b.reference.as_deref(), Some("參考答案"));
    }

    #[test]
    fn the_hash_is_sha256_of_the_exact_file_bytes() {
        // 預期值用 `shasum -a 256` 算出來,不是用這支程式自己算的:拿程式比對自己,算錯也看不出來。
        let text = format!("{}\n", line("a", CHECK));
        let ds = Dataset::parse("ds", text.as_bytes()).unwrap();
        assert_eq!(
            ds.sha256,
            "663af691aa19ffae12ec729763debbd2f0ea99bd1cf818059169e3a7a9a5a7b1"
        );

        // 多一行空行,案例一樣,hash 就不一樣:比的是檔案本身。
        let padded = format!("{text}\n");
        let other = Dataset::parse("ds", padded.as_bytes()).unwrap();
        assert_ne!(other.sha256, ds.sha256);
    }

    #[test]
    fn every_problem_is_reported_with_its_line_number() {
        let text = [
            line("a", CHECK),
            line("a", CHECK),
            line("b", r#","rubirc":["typo"]"#),
            line("c", ""),
            line("d", CHECK).replace(r#""max_tokens":64"#, r#""max_tokens":0"#),
            "{not json".to_owned(),
        ]
        .join("\n");

        let problems = problems(&text);

        assert_eq!(problems.len(), 5, "{problems:#?}");
        assert_eq!(
            problems[0],
            "line 2: duplicate id `a` (first used on line 1)"
        );
        assert!(
            problems[1].starts_with("line 3, column ")
                && problems[1].contains("unknown field `rubirc`")
                // serde_json 自己的「at line 1」會誤導人(每一行分開解析),要拿掉。
                && !problems[1].contains(" at line "),
            "{}",
            problems[1]
        );
        assert_eq!(
            problems[2],
            "line 4 (`c`): needs at least one check or rubric item, otherwise it cannot be scored"
        );
        assert_eq!(problems[3], "line 5 (`d`): max_tokens must be at least 1");
        assert!(
            problems[4].starts_with("line 6, column "),
            "{}",
            problems[4]
        );
    }

    #[test]
    fn ids_must_be_present_and_trimmed() {
        let text = [line("", CHECK), line(" x", CHECK)].join("\n");
        assert_eq!(
            problems(&text),
            [
                "line 1: id must not be empty",
                "line 2 (` x`): id must not start or end with spaces",
            ]
        );
    }

    #[test]
    fn empty_rubric_items_and_empty_check_values_are_rejected() {
        // `contains ""` 永遠通過、空白的標準沒辦法判斷:都是寫錯,不是想要的行為。
        let text = line(
            "a",
            r#","checks":[{"type":"contains","value":""},{"type":"regex","pattern":""}],"rubric":["ok"," "]"#,
        );
        assert_eq!(
            problems(&text),
            [
                "line 1 (`a`): check 1 has an empty value",
                "line 1 (`a`): check 2 has an empty pattern",
                "line 1 (`a`): rubric item 2 is empty",
            ]
        );
    }

    #[test]
    fn unknown_input_types_and_check_types_are_rejected() {
        let agent = line("a", CHECK).replace(r#""type":"chat""#, r#""type":"agent""#);
        let check = line("b", r#","checks":[{"type":"contians","value":"x"}]"#);
        let problems = problems(&[agent, check].join("\n"));
        assert_eq!(problems.len(), 2, "{problems:#?}");
        assert!(
            problems[0].contains("unknown variant `agent`"),
            "{}",
            problems[0]
        );
        assert!(
            problems[1].contains("unknown variant `contians`"),
            "{}",
            problems[1]
        );
    }

    #[test]
    fn unknown_fields_inside_the_input_are_rejected() {
        let text = line("a", CHECK).replace(r#""max_tokens":64"#, r#""max_tokens":64,"modle":"x""#);
        let problems = problems(&text);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("unknown field `modle`"),
            "{}",
            problems[0]
        );
    }

    #[test]
    fn an_empty_dataset_is_rejected() {
        assert_eq!(problems("\n\n"), ["the dataset has no cases"]);
    }

    #[test]
    fn invalid_utf8_is_reported_with_its_line() {
        let mut bytes = format!("{}\n", line("a", CHECK)).into_bytes();
        bytes.extend_from_slice(b"{\"id\":\"\xff\"}\n");
        let err = Dataset::parse("ds", &bytes).unwrap_err();
        let DatasetError::Invalid { problems, .. } = err else {
            panic!("{err:?}")
        };
        assert_eq!(problems, ["line 2: not valid UTF-8"]);
    }

    #[test]
    fn load_uses_the_file_stem_as_the_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rust-basics.jsonl");
        std::fs::write(&path, line("a", CHECK)).unwrap();

        let ds = Dataset::load(&path).unwrap();

        assert_eq!(ds.name, "rust-basics");
        assert_eq!(ds.cases.len(), 1);
    }

    #[test]
    fn a_missing_file_is_a_read_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.jsonl");
        let err = Dataset::load(&path).unwrap_err();
        assert!(
            matches!(&err, DatasetError::Read { path: p, .. } if *p == path),
            "{err:?}"
        );
    }

    #[test]
    fn the_error_message_lists_each_problem_on_its_own_line() {
        let err = Dataset::parse("ds", b"\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid dataset `ds`:\n  - the dataset has no cases"
        );
    }

    #[test]
    fn a_case_survives_a_round_trip() {
        // `eval add`(第 9 步)會把案例寫回檔案:寫出去再讀回來要一模一樣。
        let text = line(
            "b",
            r#","tags":["rust"],"rubric":["說明 move"],"reference":"參考答案""#,
        );
        let case = Dataset::parse("ds", text.as_bytes())
            .unwrap()
            .cases
            .remove(0);

        let written = serde_json::to_string(&case).unwrap();
        let again = Dataset::parse("ds", written.as_bytes())
            .unwrap()
            .cases
            .remove(0);

        assert_eq!(again, case);
    }

    #[test]
    fn debug_does_not_print_the_content() {
        // 私有資料集是真實對話:案例被印進 log 時,只能看到結構。
        let text = line(
            "a",
            r#","rubric":["RUBRIC-OK"],"reference":"REFERENCE-CANARY""#,
        )
        .replace(r#""text":"hi""#, r#""text":"USER-CANARY""#)
        .replace(
            r#""max_tokens":64"#,
            r#""max_tokens":64,"system":"SYSTEM-CANARY""#,
        );
        let case = Dataset::parse("ds", text.as_bytes())
            .unwrap()
            .cases
            .remove(0);

        let out = format!("{case:?}");

        assert!(!out.contains("CANARY"), "content leaked: {out}");
        assert!(out.contains(r#"id: "a""#), "{out}");
        assert!(out.contains("reference_bytes: Some(16)"), "{out}");
        // 不洩漏之外,還要印得出結構(cargo-mutants:`ChatInput` 的 Debug 換成什麼都不印時,原本沒有測試失敗)。
        assert!(
            out.contains("Chat(ChatInput { system_bytes: Some(13), messages: ["),
            "{out}"
        );
    }

    #[test]
    fn to_request_adds_the_model_chosen_at_run_time() {
        let text = line("a", CHECK);
        let case = Dataset::parse("ds", text.as_bytes())
            .unwrap()
            .cases
            .remove(0);
        let Input::Chat(input) = &case.input;

        let request = input.to_request("claude-haiku-5-5");

        assert_eq!(request.model, "claude-haiku-5-5");
        assert_eq!(request.max_tokens, 64);
        assert_eq!(
            request.messages[0].content,
            [Content::Text {
                text: "hi".into(),
                meta: Default::default()
            }]
        );
    }
}
