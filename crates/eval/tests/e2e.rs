//! E2E:真的執行 eval 程式(design §10),上游的 Claude 換成 wiremock。
//!
//! ```text
//! 測試 ──子行程──→ eval run / score / compare / add ──真的 HTTP──→ wiremock(假 Claude)
//! ```
//!
//! 單元測試碰不到 `main.rs`(讀設定檔、讀 API key、組裝 provider、印出結果);這裡補上。
//! 假 Claude 依題目決定回答:`cheap`(claude-haiku-5-5)答錯 JSON 那一題,`default`(claude-sonnet-5-5)全對,
//! 所以比較報告的數字是可以預期的。評審的請求(system 裡有 "strict grader")回覆逐條通過,
//! 只有校準集故意改壞的回答例外(見 `judge_reply`)。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const KEY: &str = "sk-ant-e2e-fake-key";
/// 一個指令最多跑多久。錯的實作要讓測試「失敗」,而不是「卡住」。
const PATIENCE: Duration = Duration::from_secs(60);

/// 假的 Claude:看請求決定回答。
struct FakeClaude;

impl Respond for FakeClaude {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let model = body["model"].as_str().unwrap_or_default().to_owned();
        let system = body["system"].as_str().unwrap_or_default();
        let question = body["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        let text = if system.contains("strict grader") {
            judge_reply(&model, question)
        } else {
            answer(&model, question)
        };
        sse(&model, &text)
    }
}

/// 被測模型的回答。`cheap` 的 JSON 那一題答錯(沒有 version 欄位)。
fn answer(model: &str, question: &str) -> String {
    let cheap = model == "claude-haiku-5-5";
    if question.contains("2 + 3") {
        "5".into()
    } else if question.contains("JSON object") {
        if cheap {
            r#"{"name":"serde"}"#.into()
        } else {
            r#"{"name":"serde","version":"1.0.228"}"#.into()
        }
    } else if question.contains("keyword") {
        "let".into()
    } else if question.contains("capital of France") {
        "Paris".into()
    } else {
        format!("an answer about: {question}")
    }
}

/// 評審的回覆:「## Criteria」那一段有幾條就回幾條。
/// 被評的回答裡有 `BREAKS<n>`(校準集故意改壞的)就判第 n 條不通過;
/// 但 Haiku 看不出標了 `SUBTLE` 的錯。其他情況全部通過。
fn judge_reply(model: &str, prompt: &str) -> String {
    let answer = prompt
        .split("## Answer to grade")
        .nth(1)
        .unwrap_or_default();
    let blind = model == "claude-haiku-5-5" && answer.contains("SUBTLE");
    let criteria = prompt
        .split("## Criteria\n")
        .nth(1)
        .unwrap_or_default()
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .lines()
        .count();
    let verdicts: Vec<Value> = (1..=criteria)
        .map(|n| {
            let broken = answer.contains(&format!("BREAKS{n}"));
            json!({ "criterion": n, "passed": !broken || blind, "reason": "ok" })
        })
        .collect();
    json!({ "verdicts": verdicts }).to_string()
}

/// Anthropic 的 SSE 回應(同 gateway 的 E2E)。
fn sse(model: &str, text: &str) -> ResponseTemplate {
    let events = [
        json!({ "type": "message_start", "message": { "id": "msg_1", "model": model, "usage": { "input_tokens": 100, "output_tokens": 1 } } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 50 } }),
        json!({ "type": "message_stop" }),
    ];
    let body: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// 一個測試用的工作目錄:設定檔、資料目錄、紀錄目錄都在這裡。
struct Lab {
    dir: TempDir,
    upstream: MockServer,
}

impl Lab {
    async fn start() -> Self {
        let upstream = MockServer::start().await;
        // 一定要帶 API key(證明 main 讀了環境變數、組裝了 provider)。
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", KEY))
            .respond_with(FakeClaude)
            .mount(&upstream)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let evals = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals");
        let config = format!(
            r#"
[aliases]
default = "claude-sonnet-5-5"
cheap = "claude-haiku-5-5"
[anthropic]
base_url = "{}"
[judge]
alias = "default"
[paths]
data_dir = "data"
records = "records"
datasets = "{}"
[prices."claude-haiku-5-5"]
input = 0.10
output = 0.50
[prices."claude-sonnet-5-5"]
input = 2.0
output = 10.0
"#,
            upstream.uri(),
            evals.display()
        );
        std::fs::write(dir.path().join("eval.toml"), config).unwrap();
        Self { dir, upstream }
    }

    /// 執行 eval。工作目錄是暫存目錄:不會讀到 repo 裡的 `.env`,也不在 git repo 裡。
    async fn eval(&self, args: &[&str]) -> Output {
        self.eval_with_key(args, Some(KEY)).await
    }

    async fn eval_with_key(&self, args: &[&str], key: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_eval"));
        cmd.args(args)
            .current_dir(self.dir.path())
            .env_remove("ANTHROPIC_API_KEY")
            .env("RUST_BACKTRACE", "0");
        if let Some(key) = key {
            cmd.env("ANTHROPIC_API_KEY", key);
        }
        tokio::time::timeout(PATIENCE, cmd.output())
            .await
            .expect("eval did not finish in time")
            .unwrap()
    }

    /// 成功執行,回傳 stdout。失敗時把 stdout 與 stderr 都印出來。
    async fn ok(&self, args: &[&str]) -> String {
        let out = self.eval(args).await;
        let (stdout, stderr) = text(&out);
        assert!(
            out.status.success(),
            "eval {args:?} failed:\n{stdout}\n{stderr}"
        );
        stdout
    }

    async fn calls(&self) -> usize {
        self.upstream.received_requests().await.unwrap().len()
    }
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `run: <執行目錄>` 那一行。
fn run_dir(stdout: &str) -> PathBuf {
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("run: "))
        .unwrap_or_else(|| panic!("no run directory in:\n{stdout}"));
    PathBuf::from(line)
}

#[tokio::test]
async fn run_score_and_compare_two_models_on_the_smoke_dataset() {
    let lab = Lab::start().await;

    // 估價不呼叫模型。
    let dry = lab
        .ok(&["run", "--dataset", "smoke", "--model", "cheap", "--dry-run"])
        .await;
    assert!(
        dry.starts_with("dry run: 4 call(s), worst-case cost $"),
        "{dry}"
    );
    assert_eq!(lab.calls().await, 0);

    let out_a = lab
        .ok(&["run", "--dataset", "smoke", "--model", "cheap"])
        .await;
    let a = run_dir(&out_a);
    assert!(
        out_a.contains("planned 4, ok 4, errors 0, missing 0"),
        "{out_a}"
    );
    // 每次呼叫 100 個輸入、50 個輸出 token:(100 × 0.10 + 50 × 0.50) / 1e6 × 4 = $0.00014。
    assert!(out_a.contains("cost $0.0001"), "{out_a}");
    assert!(
        out_a.contains(&format!("next: eval score {}", a.display())),
        "{out_a}"
    );
    // 執行目錄在設定檔指定的資料目錄底下,名字有資料集與模型。
    assert!(
        a.starts_with(lab.dir.path().join("data/runs")),
        "{}",
        a.display()
    );
    assert!(
        a.to_str().unwrap().ends_with("_smoke_cheap"),
        "{}",
        a.display()
    );
    let meta: Value =
        serde_json::from_str(&std::fs::read_to_string(a.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["config"]["model"], "cheap");
    // 暫存目錄不在 git repo 裡:不記 commit(不是錯誤)。
    assert!(meta.get("git_commit").is_none(), "{meta}");

    let b = run_dir(
        &lab.ok(&["run", "--dataset", "smoke", "--model", "default"])
            .await,
    );
    assert_eq!(lab.calls().await, 8);

    // 只做確定性檢查:不叫評審、不需要評審。
    let score_a = lab.ok(&["score", a.to_str().unwrap(), "--no-judge"]).await;
    assert!(score_a.contains("scores `checks`"), "{score_a}");
    assert!(
        score_a.contains("pass 3, fail 1, incomplete 0, call errors 0, missing 0"),
        "{score_a}"
    );
    assert!(
        score_a.contains("judge calls 0 (skipped 0), cost $0.0000"),
        "{score_a}"
    );
    lab.ok(&["score", b.to_str().unwrap(), "--no-judge"]).await;
    assert_eq!(lab.calls().await, 8);

    // 比較不需要 --scores:兩邊都只有 `checks` 這一份。
    let report = lab
        .ok(&["compare", a.to_str().unwrap(), b.to_str().unwrap()])
        .await;
    assert!(
        report.contains("scores `checks`: checks only\n"),
        "{report}"
    );
    assert!(
        report.contains("all-pass         75.0%  100.0%"),
        "{report}"
    );
    assert!(report.contains("improved (A -> B):"), "{report}");
    assert!(report.contains("json-001"), "{report}");
    assert!(
        report.contains("warning: only 4 paired case(s)"),
        "{report}"
    );
    assert!(!report.contains("NOISE FLOOR"), "{report}");
}

#[tokio::test]
async fn the_same_settings_twice_are_labelled_as_the_noise_floor() {
    let lab = Lab::start().await;
    let a = run_dir(
        &lab.ok(&["run", "--dataset", "smoke", "--model", "cheap"])
            .await,
    );
    // 同一秒、同一組設定會撞到同一個目錄名稱(刻意拒絕),等一秒再跑第二次。
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let b = run_dir(
        &lab.ok(&[
            "run",
            "--dataset",
            "smoke",
            "--model",
            "cheap",
            "--label",
            "again",
        ])
        .await,
    );
    for dir in [&a, &b] {
        lab.ok(&["score", dir.to_str().unwrap(), "--no-judge"])
            .await;
    }

    let report = lab
        .ok(&["compare", a.to_str().unwrap(), b.to_str().unwrap()])
        .await;

    assert!(report.contains("NOISE FLOOR"), "{report}");
}

#[tokio::test]
async fn the_judge_grades_the_rubric_and_warns_when_judging_itself() {
    let lab = Lab::start().await;
    let dataset = lab.dir.path().join("graded.jsonl");
    std::fs::write(
        &dataset,
        r#"{"id":"q1","input":{"type":"chat","max_tokens":64,"messages":[{"role":"user","content":[{"type":"text","text":"Explain move"}]}]},"rubric":["mentions ownership","no invented API"]}
{"id":"q2","input":{"type":"chat","max_tokens":64,"messages":[{"role":"user","content":[{"type":"text","text":"Explain borrow"}]}]},"rubric":["mentions references"]}
"#,
    )
    .unwrap();
    let run = run_dir(
        &lab.ok(&[
            "run",
            "--dataset",
            dataset.to_str().unwrap(),
            "--model",
            "cheap",
        ])
        .await,
    );

    // 有評分標準就不能 --no-judge。
    let out = lab
        .eval(&["score", run.to_str().unwrap(), "--no-judge"])
        .await;
    assert!(!out.status.success());
    assert!(
        text(&out).1.contains("has a rubric, which needs a judge"),
        "{:?}",
        text(&out)
    );

    // 估價不需要 API key(只有真的要叫評審時才讀)。
    let out = lab
        .eval_with_key(&["score", run.to_str().unwrap(), "--dry-run"], None)
        .await;
    let (dry, stderr) = text(&out);
    assert!(out.status.success(), "{dry}\n{stderr}");
    assert!(
        dry.starts_with("dry run: 2 call(s), worst-case cost $"),
        "{dry}"
    );
    assert_eq!(lab.calls().await, 2);

    let out = lab.eval(&["score", run.to_str().unwrap()]).await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    // 預設評審是 [judge] alias(default),打分名稱也是它。
    assert!(stdout.contains("scores `default`"), "{stdout}");
    assert!(stdout.contains("pass 2, fail 0"), "{stdout}");
    assert!(stdout.contains("judge calls 2 (skipped 0)"), "{stdout}");
    assert!(!stderr.contains("warning"), "{stderr}");
    assert_eq!(lab.calls().await, 4);

    // 評審跟被評的是同一個模型:警告(自我偏好)。
    let out = lab
        .eval(&["score", run.to_str().unwrap(), "--judge", "cheap"])
        .await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("scores `cheap`"), "{stdout}");
    assert!(
        stderr.contains("warning: the judge and the graded model are the same"),
        "{stderr}"
    );
}

#[tokio::test]
async fn mistakes_are_refused_before_anything_is_called() {
    let lab = Lab::start().await;

    // 沒有 API key:要真的呼叫模型時才讀,讀不到就停下(估價不需要)。
    let out = lab
        .eval_with_key(&["run", "--dataset", "smoke", "--model", "cheap"], None)
        .await;
    assert!(!out.status.success());
    assert!(
        text(&out)
            .1
            .contains("the API key is needed to call the model"),
        "{:?}",
        text(&out)
    );
    let dry = lab
        .eval_with_key(
            &["run", "--dataset", "smoke", "--model", "cheap", "--dry-run"],
            None,
        )
        .await;
    assert!(dry.status.success(), "{:?}", text(&dry));

    // 別名不在 [aliases] 裡。
    let out = lab
        .eval(&["run", "--dataset", "smoke", "--model", "opus"])
        .await;
    assert!(!out.status.success());
    assert!(
        text(&out)
            .1
            .contains("unknown model alias `opus`; [aliases] in the config has: cheap, default"),
        "{:?}",
        text(&out)
    );

    // 沒有設定檔。
    let out = lab
        .eval(&[
            "--config",
            "missing.toml",
            "run",
            "--dataset",
            "smoke",
            "--model",
            "cheap",
        ])
        .await;
    assert!(!out.status.success());
    assert!(
        text(&out).1.contains("cp eval.example.toml eval.toml"),
        "{:?}",
        text(&out)
    );

    assert_eq!(lab.calls().await, 0);
    assert!(!lab.dir.path().join("data/runs").exists());
}

#[tokio::test]
async fn a_budget_stops_the_run_and_resume_finishes_it() {
    let lab = Lab::start().await;
    // 一次一個呼叫;預算比一次呼叫的費用($0.000035)還少:第一個呼叫之後就停。
    let out = lab
        .eval(&[
            "run",
            "--dataset",
            "smoke",
            "--model",
            "cheap",
            "--concurrency",
            "1",
            "--budget",
            "0.00001",
        ])
        .await;
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("planned 4, ok 1, errors 0, missing 3"),
        "{stdout}"
    );
    assert!(
        stderr.contains("incomplete: 3 call(s) were not made"),
        "{stderr}"
    );
    let dir = run_dir(&stdout);

    let dry = lab
        .ok(&["run", "--resume", dir.to_str().unwrap(), "--dry-run"])
        .await;
    assert!(dry.starts_with("dry run: 3 call(s)"), "{dry}");

    let resumed = lab.ok(&["run", "--resume", dir.to_str().unwrap()]).await;
    assert!(
        resumed.contains("planned 4, ok 4, errors 0, missing 0"),
        "{resumed}"
    );
    assert_eq!(lab.calls().await, 4);
}

#[tokio::test]
async fn a_call_record_becomes_a_private_dataset_case() {
    let lab = Lab::start().await;
    let id = "019a6f2e-5c00-7abc-8def-0123456789ab";
    // UUID v7 的前 48 位元是毫秒時間戳(0x019a6f2e5c00)。
    let record = json!({
        "schema_version": 1, "id": id, "started_at": "2025-11-08T00:00:00Z",
        "requested_model": "cheap", "served_model": "claude-haiku-5-5",
        "request": { "model": "cheap", "max_tokens": 256,
                     "messages": [{ "role": "user", "content": [{ "type": "text", "text": "Rust 的 move 是什麼?" }] }] },
        "response": { "model": "claude-haiku-5-5", "content": [{ "type": "text", "text": "所有權轉移" }],
                      "stop": { "kind": "end_turn", "raw": "end_turn" }, "usage": { "input_tokens": 1, "output_tokens": 1 } },
        "outcome": "ok", "error": null, "usage": { "input_tokens": 1, "output_tokens": 1 },
        "latency_ms": { "first_output": null, "total": 10 }
    });
    // 紀錄檔是 id 那一天(UTC)的檔案。
    let ts = uuid::Uuid::parse_str(id)
        .unwrap()
        .get_timestamp()
        .unwrap()
        .to_unix();
    let day = jiff::Timestamp::from_second(i64::try_from(ts.0).unwrap())
        .unwrap()
        .strftime("%Y-%m-%d")
        .to_string();
    std::fs::create_dir_all(lab.dir.path().join("records")).unwrap();
    std::fs::write(
        lab.dir.path().join(format!("records/{day}.jsonl")),
        record.to_string() + "\n",
    )
    .unwrap();
    let args = [
        "add",
        "--record",
        id,
        "--dataset",
        "personal",
        "--rubric",
        "說明所有權轉移",
        "--keep-answer",
    ];

    // --print:只印出來,不寫檔。
    let printed = lab.ok(&[&args[..], &["--print"]].concat()).await;
    let case: Value = serde_json::from_str(&printed).unwrap();
    assert_eq!(case["id"], format!("rec-{id}"));
    assert_eq!(case["reference"], "所有權轉移");
    let path = lab.dir.path().join("data/datasets/personal.jsonl");
    assert!(!path.exists());

    let added = lab.ok(&args).await;
    assert_eq!(
        added.trim(),
        format!("added case `rec-{id}` to {}", path.display())
    );
    // 剛加的私有資料集馬上可以用名稱跑。
    let dry = lab
        .ok(&[
            "run",
            "--dataset",
            "personal",
            "--model",
            "cheap",
            "--dry-run",
        ])
        .await;
    assert!(dry.starts_with("dry run: 1 call(s)"), "{dry}");
}

/// 一個小資料集與它的校準集:對照(Sonnet 寫的)、微妙的錯(Haiku 寫的,第 1 條)、明顯的錯(Sonnet 寫的,第 2 條)。
fn calibration_lab(lab: &Lab) -> PathBuf {
    let dataset = lab.dir.path().join("mini.jsonl");
    std::fs::write(
        &dataset,
        r#"{"id":"a","input":{"type":"chat","max_tokens":64,"messages":[{"role":"user","content":[{"type":"text","text":"Explain move"}]}]},"rubric":["mentions ownership","no invented API"]}
"#,
    )
    .unwrap();
    let dir = lab.dir.path().join("calibration");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(
        dir.join("mini.jsonl"),
        r#"{"id":"c1","case":"a","source_model":"claude-sonnet-5-5","kind":"control","answer":"ownership moves","expected":{"mentions ownership":"pass","no invented API":"pass"}}
{"id":"s1","case":"a","source_model":"claude-haiku-5-5","kind":"subtle","change":"copies","answer":"it copies BREAKS1 SUBTLE","expected":{"mentions ownership":"fail","no invented API":"pass"}}
{"id":"o1","case":"a","source_model":"claude-sonnet-5-5","kind":"fabricated","change":"made-up API","answer":"call String::move_to() BREAKS2","expected":{"mentions ownership":"pass","no invented API":"fail"}}
"#,
    )
    .unwrap();
    dataset
}

/// 報告裡以 `prefix` 開頭的那一行。
fn row<'a>(report: &'a str, prefix: &str) -> &'a str {
    report
        .lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no line starting with {prefix:?} in:\n{report}"))
}

#[tokio::test]
async fn calibrate_measures_each_judge_on_planted_mistakes() {
    let lab = Lab::start().await;
    let dataset = calibration_lab(&lab);
    let dataset = dataset.to_str().unwrap();

    // 估價:不呼叫評審,也不需要 API key。2 個評審 × 3 筆。
    let out = lab
        .eval_with_key(
            &[
                "calibrate",
                "--dataset",
                dataset,
                "--judge",
                "default",
                "--judge",
                "cheap",
                "--dry-run",
            ],
            None,
        )
        .await;
    let (dry, stderr) = text(&out);
    assert!(out.status.success(), "{dry}\n{stderr}");
    assert!(
        dry.starts_with("dry run: 6 call(s), worst-case cost $"),
        "{dry}"
    );
    assert_eq!(lab.calls().await, 0);

    let report = lab
        .ok(&[
            "calibrate",
            "--dataset",
            dataset,
            "--judge",
            "default",
            "--judge",
            "cheap",
            "--budget",
            "1.00",
        ])
        .await;

    assert_eq!(lab.calls().await, 6);
    assert!(
        report.contains("Calibrate mini (3 answers, 6 criteria: 2 expected fail, 4 expected pass)"),
        "{report}"
    );
    // default(Sonnet)兩個錯都抓到;cheap(Haiku)看不出 SUBTLE 的那個。
    let default = row(&report, "default  ");
    assert!(
        default.contains("  2/2") && default.contains("  0/4"),
        "{default}"
    );
    let cheap = row(&report, "cheap  ");
    assert!(
        cheap.contains("  1/2") && cheap.contains("  0/4"),
        "{cheap}"
    );
    assert!(
        report.contains("  cheap    s1  mentions ownership  (ok)"),
        "{report}"
    );
    // 自我偏好:Haiku 寫的只有 s1,cheap 剛好漏掉它。
    assert!(
        row(&report, "cheap    own").contains("own 0/1 (0.0%)"),
        "{report}"
    );
    assert!(report.contains("cost: default $"), "{report}");

    // 原始判決檔:私有(600),一個評審一筆回答一行。
    let results = report
        .lines()
        .find_map(|l| l.strip_prefix("results: "))
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("no results path in:\n{report}"));
    assert!(
        results.starts_with(lab.dir.path().join("data/calibrations")),
        "{}",
        results.display()
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&results).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let lines: Vec<Value> = std::fs::read_to_string(&results)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let keys: Vec<(&str, &str)> = lines
        .iter()
        .map(|l| (l["judge"].as_str().unwrap(), l["item"].as_str().unwrap()))
        .collect();
    assert_eq!(
        keys,
        [
            ("default", "c1"),
            ("default", "s1"),
            ("default", "o1"),
            ("cheap", "c1"),
            ("cheap", "s1"),
            ("cheap", "o1")
        ]
    );
    assert_eq!(
        lines[4]["verdicts"][0]["result"], "pass",
        "cheap 漏掉 s1 的第 1 條"
    );
}

#[tokio::test]
async fn calibrate_refuses_mistakes_before_calling_a_judge() {
    let lab = Lab::start().await;
    let dataset = calibration_lab(&lab);
    let dataset = dataset.to_str().unwrap();

    let refused = |out: &Output, needle: &str| {
        let (stdout, stderr) = text(out);
        assert!(!out.status.success(), "{stdout}\n{stderr}");
        assert!(stderr.contains(needle), "expected {needle:?} in:\n{stderr}");
    };
    refused(
        &lab.eval(&[
            "calibrate",
            "--dataset",
            dataset,
            "--judge",
            "cheap",
            "--judge",
            "cheap",
        ])
        .await,
        "--judge `cheap` is given twice",
    );
    refused(
        &lab.eval(&["calibrate", "--dataset", dataset, "--judge", "opus"])
            .await,
        "unknown model alias `opus`",
    );
    // smoke 沒有校準集。
    refused(
        &lab.eval(&["calibrate", "--dataset", "smoke"]).await,
        "dataset `smoke` has no calibration set; expected ",
    );
    assert_eq!(lab.calls().await, 0);
    assert!(!lab.dir.path().join("data/calibrations").exists());
}
