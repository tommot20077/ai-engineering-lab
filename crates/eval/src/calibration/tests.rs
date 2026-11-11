use serde_json::{Value, json};

use super::*;

const R1: &str = "說明所有權會轉移";
const R2: &str = "沒有捏造不存在的 API";

/// 兩題的資料集:`a` 有兩條評分標準;`b` 只有確定性檢查(沒有東西可以校準)。
fn dataset() -> Dataset {
    let a = json!({"id": "a", "input": chat(), "rubric": [R1, R2]});
    let b = json!({"id": "b", "input": chat(), "checks": [{"type": "contains", "value": "x"}]});
    Dataset::parse("ds", format!("{a}\n{b}\n").as_bytes()).unwrap()
}

fn chat() -> Value {
    json!({"type": "chat", "max_tokens": 64,
           "messages": [{"role": "user", "content": [{"type": "text", "text": "q"}]}]})
}

/// 一行合法的「改壞」的回答(R1 應該不通過)。`edit` 用來改寫或拿掉欄位。
fn item(id: &str, edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> String {
    let mut v = json!({
        "id": id, "case": "a", "source_model": "claude-haiku-5-5", "kind": "subtle",
        "change": "把「轉移」改成「複製」", "answer": "move 會複製一份。",
        "expected": {R1: "fail", R2: "pass"}
    });
    edit(v.as_object_mut().unwrap());
    v.to_string()
}

fn problems(text: &str) -> Vec<String> {
    match CalibrationSet::parse(text.as_bytes(), &dataset()) {
        Err(CalibrationError::Invalid { dataset, problems }) => {
            assert_eq!(dataset, "ds");
            problems
        }
        other => panic!("應該被拒絕:{other:?}"),
    }
}

#[test]
fn parses_every_kind_and_orders_labels_by_the_rubric() {
    let control = item("c", |m| {
        m.insert("kind".into(), json!("control"));
        m.remove("change");
        // serde_json 的 map 依 key 排序,R2(「沒」)會排在 R1(「說」)前面,跟評分標準相反:讀進來要依評分標準的順序排。
        m.insert("expected".into(), json!({R2: "pass", R1: "pass"}));
    });
    let mut lines = vec![control, String::new()]; // 空行略過
    for kind in ["obvious", "subtle", "fabricated", "off_topic", "injection"] {
        lines.push(item(kind, |m| {
            m.insert("kind".into(), json!(kind));
        }));
    }

    let set = CalibrationSet::parse(lines.join("\n").as_bytes(), &dataset()).unwrap();

    assert_eq!(set.dataset, "ds");
    let kinds: Vec<Kind> = set.items.iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        [
            Kind::Control,
            Kind::Obvious,
            Kind::Subtle,
            Kind::Fabricated,
            Kind::OffTopic,
            Kind::Injection
        ]
    );
    let label = |criterion: &str, expected| Label {
        criterion: criterion.into(),
        expected,
    };
    assert_eq!(
        set.items[0],
        Item {
            id: "c".into(),
            case_id: "a".into(),
            source_model: "claude-haiku-5-5".into(),
            kind: Kind::Control,
            change: None,
            answer: "move 會複製一份。".into(),
            labels: vec![label(R1, Expected::Pass), label(R2, Expected::Pass)],
        }
    );
    assert_eq!(
        set.items[2].change.as_deref(),
        Some("把「轉移」改成「複製」")
    );
    assert_eq!(
        set.items[2].labels,
        [label(R1, Expected::Fail), label(R2, Expected::Pass)]
    );
}

#[test]
fn lists_every_problem_with_its_line_number() {
    let text = [
        item("a1", |m| {
            m.insert("note".into(), json!("多的欄位"));
        }),
        item("a2", |_| {}),
        item("a2", |_| {}),
        item("a3", |m| {
            m.insert("case".into(), json!("nope"));
        }),
    ]
    .join("\n");

    let p = problems(&text);

    assert_eq!(p.len(), 3, "{p:#?}");
    assert!(p[0].starts_with("line 1, column "), "{}", p[0]);
    assert!(p[0].contains("unknown field `note`"), "{}", p[0]);
    assert_eq!(p[1], "line 3: duplicate id `a2` (first used on line 2)");
    assert_eq!(p[2], "line 4 (`a3`): unknown case `nope` in dataset `ds`");
}

#[test]
fn a_case_without_a_rubric_has_nothing_to_calibrate() {
    let p = problems(&item("x", |m| {
        m.insert("case".into(), json!("b"));
    }));
    assert_eq!(
        p,
        ["line 1 (`x`): case `b` has no rubric, so there is nothing for a judge to calibrate"]
    );
}

#[test]
fn labels_must_name_exactly_the_criteria_of_the_case() {
    // 評分標準改了字,舊的標籤就對不上:要拒絕,不能默默拿舊的標籤比新的標準。
    let p = problems(&item("x", |m| {
        m.insert(
            "expected".into(),
            json!({"說明所有權會移動": "fail", R2: "pass"}),
        );
    }));
    assert_eq!(
        p,
        [
            "line 1 (`x`): `expected` names a criterion that case `a` does not have: \"說明所有權會移動\"",
            "line 1 (`x`): `expected` is missing the criterion \"說明所有權會轉移\"",
        ]
    );
}

#[test]
fn a_criterion_named_twice_is_rejected() {
    // serde 讀成 map 時,重複的 key 會被後面的默默蓋掉;這裡要報錯。
    let line = item("x", |_| {}).replace(
        &format!("\"{R2}\":\"pass\""),
        &format!("\"{R2}\":\"pass\",\"{R1}\":\"pass\""),
    );
    assert!(line.contains(&format!("\"{R1}\":\"pass\"")), "{line}");

    let p = problems(&line);

    assert_eq!(p.len(), 1, "{p:#?}");
    assert!(
        p[0].contains(&format!("criterion \"{R1}\" is listed twice")),
        "{}",
        p[0]
    );
}

#[test]
fn a_label_is_pass_or_fail() {
    let p = problems(&item("x", |m| {
        m.insert("expected".into(), json!({R1: "maybe", R2: "pass"}));
    }));
    assert_eq!(p.len(), 1, "{p:#?}");
    assert!(p[0].contains("unknown variant `maybe`"), "{}", p[0]);
}

#[test]
fn a_control_expects_every_criterion_to_pass_and_has_no_change() {
    let p = problems(&item("x", |m| {
        m.insert("kind".into(), json!("control"));
    }));
    assert_eq!(
        p,
        [
            "line 1 (`x`): a control answer must expect every criterion to pass",
            "line 1 (`x`): a control answer was not changed, so it has no `change`",
        ]
    );
}

#[test]
fn a_changed_answer_expects_a_failure_and_says_what_changed() {
    let p = problems(&item("x", |m| {
        m.insert("expected".into(), json!({R1: "pass", R2: "pass"}));
        m.insert("change".into(), json!("  "));
    }));
    assert_eq!(
        p,
        [
            "line 1 (`x`): a changed answer must expect at least one criterion to fail",
            "line 1 (`x`): `change` must say what was changed",
        ]
    );
    // 沒有 `change` 欄位也一樣。
    let p = problems(&item("y", |m| {
        m.remove("change");
    }));
    assert_eq!(p, ["line 1 (`y`): `change` must say what was changed"]);
}

#[test]
fn id_answer_and_source_model_cannot_be_blank() {
    let p = problems(&item("", |m| {
        m.insert("answer".into(), json!(" \n"));
        m.insert("source_model".into(), json!(""));
    }));
    assert_eq!(
        p,
        [
            "line 1: `id` is empty",
            "line 1: `answer` is empty",
            "line 1: `source_model` is empty",
        ]
    );
}

#[test]
fn the_error_message_lists_each_problem_on_its_own_line() {
    // 校準集寫錯時,使用者讀的就是這段訊息(cargo-mutants 在第 2 步找到:把清單換成空字串,沒有測試會失敗)。
    let text = [
        item("", |_| {}),
        item("x", |m| {
            m.insert("case".into(), json!("nope"));
        }),
    ]
    .join("\n");
    let err = CalibrationSet::parse(text.as_bytes(), &dataset()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid calibration set for dataset `ds`:\n  - line 1: `id` is empty\n  - line 2 (`x`): unknown case `nope` in dataset `ds`"
    );
}

#[test]
fn labels_must_be_an_object() {
    // serde 的型別錯誤訊息會接上 visitor 的 `expecting`:要說清楚需要什麼(第 2 步 cargo-mutants 找到的缺口)。
    let p = problems(&item("x", |m| {
        m.insert("expected".into(), json!(["fail", "pass"]));
    }));
    assert_eq!(p.len(), 1, "{p:#?}");
    assert!(
        p[0].contains("expected an object mapping each criterion to \"pass\" or \"fail\""),
        "{}",
        p[0]
    );
}

#[test]
fn an_empty_set_is_rejected() {
    assert_eq!(problems("\n\n"), ["the calibration set has no answers"]);
}

#[test]
fn a_missing_file_names_the_path_and_keeps_the_cause_in_the_chain() {
    let err =
        CalibrationSet::load(Path::new("/no/such/calibration.jsonl"), &dataset()).unwrap_err();
    // 原因只出現在錯誤鏈裡,不重複印在訊息裡。
    assert_eq!(
        err.to_string(),
        "cannot read calibration set /no/such/calibration.jsonl"
    );
    let cause = std::error::Error::source(&err).unwrap().to_string();
    assert!(cause.contains("No such file"), "{cause}");
}

#[test]
fn the_calibration_set_lives_next_to_its_dataset() {
    assert_eq!(
        CalibrationSet::path_for(Path::new("/repo/evals/rust-basics.jsonl")).to_str(),
        Some("/repo/evals/calibration/rust-basics.jsonl")
    );
    assert_eq!(
        CalibrationSet::path_for(Path::new("/data/datasets/personal.jsonl")).to_str(),
        Some("/data/datasets/calibration/personal.jsonl")
    );
}

#[test]
fn an_answer_reaches_the_judge_as_the_source_model_wrote_it() {
    let set = CalibrationSet::parse(item("x", |_| {}).as_bytes(), &dataset()).unwrap();
    let output = set.items[0].output();
    assert_eq!(output.text(), "move 會複製一份。");
    assert_eq!(
        output.model(),
        "claude-haiku-5-5",
        "自我偏好要比對原本寫這個回答的模型"
    );
}
