use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;

use jiff::Timestamp;
use llm::{
    Content, Latency, Message, ProviderMetadata, Request, Response, Role, SCHEMA_VERSION, StopKind,
    StopReason, Usage,
};
use serde_json::json;

use super::*;
use crate::case::Input;
use crate::dataset::Dataset;

fn t(s: &str) -> Timestamp {
    s.parse().unwrap()
}

/// 那個時間點產生的 UUID v7(跟 gateway 一樣:id 的時間 = 呼叫開始的時間)。
fn v7(at: &str) -> Uuid {
    let ts = t(at);
    let nanos = u32::try_from(ts.subsec_nanosecond()).unwrap();
    let secs = u64::try_from(ts.as_second()).unwrap();
    Uuid::new_v7(uuid::Timestamp::from_unix(uuid::NoContext, secs, nanos))
}

fn text(s: &str) -> Content {
    Content::Text {
        text: s.into(),
        meta: ProviderMetadata::default(),
    }
}

fn record(at: &str, outcome: Outcome) -> CallRecord {
    let mut options = ProviderMetadata::default();
    options.insert("anthropic", json!({ "temperature": 0.2 }));
    let response = Response {
        id: Some("msg_1".into()),
        model: "claude-haiku-5-5".into(),
        content: vec![text("move 會把所有權轉給新的變數")],
        stop: StopReason {
            kind: StopKind::EndTurn,
            raw: "end_turn".into(),
        },
        usage: Usage::default(),
        meta: ProviderMetadata::default(),
    };
    CallRecord {
        schema_version: SCHEMA_VERSION,
        id: v7(at),
        started_at: t(at),
        requested_model: "cheap".into(),
        served_model: Some("claude-haiku-5-5".into()),
        request: Request {
            model: "cheap".into(),
            system: Some("用繁體中文回答".into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![text("Rust 的 move 是什麼?")],
            }],
            tools: vec![],
            max_tokens: 1024,
            provider_options: options,
        },
        response: (outcome != Outcome::Error).then_some(response),
        outcome,
        error: None,
        usage: Usage::default(),
        latency_ms: Latency {
            first_output: None,
            total: 1200,
        },
    }
}

/// 寫一個紀錄檔(一行一筆),跟 gateway 的 JsonlWriter 一樣的格式。
fn write_records(dir: &Path, day: &str, records: &[&CallRecord], extra: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let mut text: String = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n")
        .collect();
    text.push_str(extra);
    std::fs::write(dir.join(format!("{day}.jsonl")), text).unwrap();
}

#[test]
fn the_record_file_is_the_day_of_the_id_then_the_days_around_it() {
    let dir = Path::new("/records");
    let id = v7("2026-11-01T23:59:59.999Z");

    assert_eq!(
        record_files(dir, id).unwrap(),
        [
            dir.join("2026-11-01.jsonl"),
            dir.join("2026-10-31.jsonl"),
            dir.join("2026-11-02.jsonl"),
        ]
    );
}

#[test]
fn ids_that_are_not_uuid_v7_are_refused() {
    // v4(隨機)沒有時間,不可能是呼叫紀錄的 id。
    let v4 = Uuid::parse_str("8f2a4c1e-3b5d-4e6f-9a7b-1c2d3e4f5a6b").unwrap();
    assert!(matches!(
        record_files(Path::new("/r"), v4),
        Err(AddError::NotCallRecordId(id)) if id == v4
    ));
    assert!(matches!(
        find_record(Path::new("/r"), Uuid::nil()),
        Err(AddError::NotCallRecordId(_))
    ));
    // v1 也有時間戳(編碼方式不同):只靠「取得出時間」不夠,版本一定要是 7。
    // (手寫 mutation:拿掉版本檢查時,只用 v4 的測試看不出來。)
    let v1 = Uuid::parse_str("c232ab00-9414-11ec-b3c8-9f6bdeced846").unwrap();
    assert!(v1.get_timestamp().is_some());
    assert!(matches!(
        record_files(Path::new("/r"), v1),
        Err(AddError::NotCallRecordId(id)) if id == v1
    ));
}

#[test]
fn a_record_is_found_by_id_skipping_half_written_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("records");
    let a = record("2026-11-01T08:00:00Z", Outcome::Ok);
    let b = record("2026-11-01T09:00:00Z", Outcome::Ok);
    // gateway 當機時可能留下寫了一半的行:讀不懂就略過,不影響其他筆。
    write_records(
        &dir,
        "2026-11-01",
        &[&a],
        "{\"schema_version\":1,\"id\":\"01\n",
    );
    let mut day = std::fs::read_to_string(dir.join("2026-11-01.jsonl")).unwrap();
    day.push_str(&(serde_json::to_string(&b).unwrap() + "\n"));
    std::fs::write(dir.join("2026-11-01.jsonl"), day).unwrap();

    assert_eq!(find_record(&dir, b.id).unwrap(), b);
    assert_eq!(find_record(&dir, a.id).unwrap(), a);
}

#[test]
fn a_record_written_just_after_midnight_is_found_in_the_next_day() {
    // id 的時間是 23:59:59.999,開始時間(決定檔名)晚了幾毫秒,已經是隔天。
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("records");
    let mut r = record("2026-11-01T23:59:59.999Z", Outcome::Ok);
    r.started_at = t("2026-11-02T00:00:00.002Z");
    write_records(&dir, "2026-11-02", &[&r], "");

    assert_eq!(find_record(&dir, r.id).unwrap(), r);
}

#[test]
fn a_missing_record_says_where_it_looked() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("records");
    let other = record("2026-11-01T08:00:00Z", Outcome::Ok);
    write_records(&dir, "2026-11-01", &[&other], "");
    let id = v7("2026-11-01T10:00:00Z");

    let err = find_record(&dir, id).unwrap_err();

    let AddError::RecordNotFound { id: got, looked } = &err else {
        panic!("{err:?}")
    };
    assert_eq!(*got, id);
    assert_eq!(looked, &record_files(&dir, id).unwrap());
}

#[test]
fn other_read_errors_are_reported_not_treated_as_missing() {
    // 檔案不存在 = 那天沒有紀錄,看下一個;讀不了(這裡是同名的目錄)要報錯。
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("records");
    std::fs::create_dir_all(dir.join("2026-11-01.jsonl")).unwrap();

    let err = find_record(&dir, v7("2026-11-01T10:00:00Z")).unwrap_err();

    assert!(
        matches!(&err, AddError::Read { path, .. } if path.ends_with("2026-11-01.jsonl")),
        "{err:?}"
    );
}

fn options(rubric: &[&str]) -> CaseOptions {
    CaseOptions {
        rubric: rubric.iter().map(|s| (*s).to_owned()).collect(),
        ..CaseOptions::default()
    }
}

#[test]
fn the_request_becomes_the_input_without_the_model() {
    let r = record("2026-11-01T08:00:00Z", Outcome::Ok);

    let case = case_from_record(
        &r,
        &CaseOptions {
            tags: vec!["rust".into()],
            ..options(&["說明 move 之後原本的變數不能再用"])
        },
    )
    .unwrap();

    assert_eq!(case.id, format!("rec-{}", r.id));
    assert_eq!(case.tags, ["rust"]);
    assert_eq!(case.rubric, ["說明 move 之後原本的變數不能再用"]);
    assert!(case.checks.is_empty());
    // 沒有 --keep-answer:不帶原本的回答。
    assert_eq!(case.reference, None);
    let Input::Chat(input) = &case.input else {
        panic!("expected a chat input");
    };
    // 送出去的請求(換上任何模型)跟原本的一模一樣:system、messages、tools、max_tokens、provider_options 都在。
    assert_eq!(input.to_request("cheap"), r.request);
}

#[test]
fn keep_answer_stores_the_original_answer_as_the_reference() {
    let r = record("2026-11-01T08:00:00Z", Outcome::Ok);
    let case = case_from_record(
        &r,
        &CaseOptions {
            id: Some("ownership-007".into()),
            keep_answer: true,
            ..options(&["r"])
        },
    )
    .unwrap();
    assert_eq!(case.id, "ownership-007");
    assert_eq!(
        case.reference.as_deref(),
        Some("move 會把所有權轉給新的變數")
    );
}

#[test]
fn keep_answer_needs_a_complete_answer() {
    // 中斷或失敗的呼叫,回答不完整或沒有:不能當參考答案(會讓評審拿錯的標準評分)。
    for outcome in [Outcome::Error, Outcome::Interrupted, Outcome::Cancelled] {
        let r = record("2026-11-01T08:00:00Z", outcome);
        let err = case_from_record(
            &r,
            &CaseOptions {
                keep_answer: true,
                ..options(&["r"])
            },
        )
        .unwrap_err();
        assert!(
            matches!(&err, AddError::NoAnswer { outcome: o, .. } if *o == outcome),
            "{err:?}"
        );
        // 不帶回答的話,輸入照樣可以變成案例(題目本身是好的)。
        assert!(case_from_record(&r, &options(&["r"])).is_ok());
    }
}

#[test]
fn the_new_case_must_be_valid() {
    let r = record("2026-11-01T08:00:00Z", Outcome::Ok);
    // 沒有評分標準就沒辦法打分(案例沒有確定性檢查)。
    let err = case_from_record(&r, &options(&[])).unwrap_err();
    assert!(
        matches!(&err, AddError::InvalidCase(p) if p == &["needs at least one check or rubric item, otherwise it cannot be scored"]),
        "{err:?}"
    );
    let err = case_from_record(&r, &options(&["  "])).unwrap_err();
    assert!(
        matches!(&err, AddError::InvalidCase(p) if p == &["rubric item 1 is empty"]),
        "{err:?}"
    );
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn case_with_id(id: &str) -> Case {
    case_from_record(
        &record("2026-11-01T08:00:00Z", Outcome::Ok),
        &CaseOptions {
            id: Some(id.into()),
            ..options(&["r"])
        },
    )
    .unwrap()
}

#[test]
fn cases_are_appended_to_a_private_dataset() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data/datasets");
    let path = dir.join("personal.jsonl");

    append_case(&path, &case_with_id("a")).unwrap();
    append_case(&path, &case_with_id("b")).unwrap();

    // 寫出來的檔案就是一個合法的資料集,讀回來一模一樣。
    let ds = Dataset::load(&path).unwrap();
    let ids: Vec<&str> = ds.cases.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["a", "b"]);
    assert!(ds.cases[0] == case_with_id("a"));
    // 一個案例剛好一行,沒有多出來的空行(讀資料集時會略過空行,所以要直接看檔案)。
    // (cargo-mutants:「最後一行沒有換行」的判斷換成永遠 true 時,每次都多寫一個換行,沒有測試失敗。)
    let expected = [case_with_id("a"), case_with_id("b")]
        .iter()
        .map(|c| serde_json::to_string(c).unwrap() + "\n")
        .collect::<String>();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn a_hand_edited_file_without_a_final_newline_still_gets_its_own_line() {
    // 手寫的資料集最後一行常常沒有換行:新的一行不能黏在它後面。
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("datasets");
    let path = dir.join("personal.jsonl");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();
    let first = serde_json::to_string(&case_with_id("a")).unwrap();
    std::fs::write(&path, &first).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();

    append_case(&path, &case_with_id("b")).unwrap();

    assert_eq!(Dataset::load(&path).unwrap().cases.len(), 2);
}

#[test]
fn a_duplicate_id_is_refused_and_nothing_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("datasets/personal.jsonl");
    append_case(&path, &case_with_id("a")).unwrap();
    let before = std::fs::read(&path).unwrap();

    let err = append_case(&path, &case_with_id("a")).unwrap_err();

    assert!(
        matches!(&err, AddError::DuplicateId { id, path: p } if id == "a" && p == &path),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn a_broken_dataset_is_not_appended_to() {
    // 既有的資料集讀不懂:不猜,也不在後面繼續加(寫入路徑 fail-loud)。
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("datasets");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("personal.jsonl");
    std::fs::write(&path, "not json\n").unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();

    let err = append_case(&path, &case_with_id("a")).unwrap_err();

    assert!(
        matches!(err, AddError::Dataset(DatasetError::Invalid { .. })),
        "{err:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json\n");
}

#[test]
fn a_directory_or_file_others_can_read_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("datasets");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();
    let path = dir.join("personal.jsonl");

    let err = append_case(&path, &case_with_id("a")).unwrap_err();
    assert!(
        matches!(&err, AddError::NotPrivate { path: p, mode: 0o755, fix: "700" } if *p == dir),
        "{err:?}"
    );
    assert!(!path.exists());

    // 目錄沒問題,但檔案別人讀得到。
    std::fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();
    append_case(&path, &case_with_id("a")).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
    let err = append_case(&path, &case_with_id("b")).unwrap_err();
    assert!(
        matches!(&err, AddError::NotPrivate { path: p, mode: 0o644, fix: "600" } if *p == path),
        "{err:?}"
    );
}
