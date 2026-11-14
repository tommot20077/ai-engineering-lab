use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Deserialize;
use serde_json::json;

use super::*;

// ---- 測試用的工具 ----

/// 把兩個整數相加,並記下被呼叫幾次:參數不合法時,工具本身不能被呼叫。
#[derive(Default)]
struct Add {
    calls: AtomicUsize,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    /// 第一個數
    a: i64,
    /// 第二個數
    b: i64,
}

impl Tool for Add {
    type Args = AddArgs;

    fn name(&self) -> &str {
        "add"
    }

    fn description(&self) -> &str {
        "把兩個整數相加"
    }

    async fn call(&self, args: AddArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok((args.a + args.b).to_string())
    }
}

/// 名稱與說明可以隨意指定,用來測 `Toolbox` 的檢查。
struct Named {
    name: String,
    description: String,
}

fn named(name: &str, description: &str) -> Box<dyn ErasedTool> {
    Box::new(Named {
        name: name.into(),
        description: description.into(),
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EchoArgs {
    text: String,
}

impl Tool for Named {
    type Args = EchoArgs;

    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call(&self, args: EchoArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        Ok(format!("{}: {}", self.name, args.text))
    }
}

/// 參數不是物件:schema 是 `{"type": "integer"}`,Anthropic 不接受。
struct Scalar;

impl Tool for Scalar {
    type Args = i64;

    fn name(&self) -> &str {
        "scalar"
    }

    fn description(&self) -> &str {
        "參數是一個整數"
    }

    async fn call(&self, args: i64, _ctx: &ToolContext) -> Result<String, ToolError> {
        Ok(args.to_string())
    }
}

/// 參數裡有巢狀的型別:schema 要直接展開,不能用 `$defs` + `$ref`。
struct Move;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Point {
    x: i64,
    y: i64,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MoveArgs {
    to: Point,
}

impl Tool for Move {
    type Args = MoveArgs;

    fn name(&self) -> &str {
        "move"
    }

    fn description(&self) -> &str {
        "移動到某一點"
    }

    async fn call(&self, args: MoveArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        Ok(format!("{} {}", args.to.x, args.to.y))
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        run_id: Uuid::nil(),
        call_id: "toolu_1".into(),
        clock: Clock::Fixed(Timestamp::UNIX_EPOCH),
    }
}

/// 斷言是 `InvalidArgs`,而且訊息以 `prefix` 開頭(serde 的訊息結尾有行號、欄號)。
fn assert_invalid_args(result: Result<String, ToolError>, prefix: &str) {
    match result {
        Err(ToolError::InvalidArgs(message)) => assert!(
            message.starts_with(prefix),
            "message {message:?} should start with {prefix:?}"
        ),
        other => panic!("expected InvalidArgs, got {other:?}"),
    }
}

// ---- 有型別的工具,經過抹除型別的那一層 ----

#[tokio::test]
async fn a_typed_tool_runs_with_parsed_arguments() {
    let add = Add::default();
    // 透過 `&dyn ErasedTool` 呼叫:證明 blanket impl 讓 `Add` 能當成抹除型別的工具用。
    let erased: &dyn ErasedTool = &add;
    let result = erased.call_json(r#"{"a": 2, "b": 3}"#, &ctx()).await;
    assert_eq!(result, Ok("5".to_string()));
    assert_eq!(add.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bad_arguments_never_reach_the_tool() {
    let add = Add::default();
    let cases = [
        (
            r#"{"a": "two", "b": 3}"#,
            "invalid type: string \"two\", expected i64",
        ),
        (r#"{"a": 1, "b": 2, "c": 3}"#, "unknown field `c`"),
        (r#"{"a": 1}"#, "missing field `b`"),
        ("not json", "expected ident"),
        // serde 的 struct 本來也接受陣列(照欄位順序填);schema 說是物件,就只收物件。
        (r#"[1, 2]"#, "arguments must be a JSON object"),
        ("5", "arguments must be a JSON object"),
        ("null", "arguments must be a JSON object"),
    ];
    for (args, prefix) in cases {
        assert_invalid_args(add.call_json(args, &ctx()).await, prefix);
    }
    assert_eq!(add.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn the_definition_comes_from_the_argument_type() {
    assert_eq!(
        Add::default().def(),
        ToolDef {
            name: "add".into(),
            description: "把兩個整數相加".into(),
            // 沒有 `$schema` 與 `title`(Rust 的型別名稱):模型用不到,只是雜訊。
            input_schema: json!({
                "type": "object",
                "properties": {
                    "a": {"description": "第一個數", "type": "integer", "format": "int64"},
                    "b": {"description": "第二個數", "type": "integer", "format": "int64"}
                },
                "required": ["a", "b"],
                "additionalProperties": false
            }),
        }
    );
}

#[test]
fn nested_argument_types_are_written_out_in_place() {
    assert_eq!(
        Move.def().input_schema,
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer", "format": "int64"},
                        "y": {"type": "integer", "format": "int64"}
                    },
                    "required": ["x", "y"],
                    "additionalProperties": false
                }
            },
            "required": ["to"],
            "additionalProperties": false
        })
    );
}

// ---- Toolbox ----

#[test]
fn the_toolbox_lists_definitions_in_registration_order() {
    // 註冊順序刻意跟字母順序相反:排序過的實作會被抓到。
    let toolbox = Toolbox::new(vec![named("zeta", "z"), named("alpha", "a")]).unwrap();
    let names: Vec<String> = toolbox.defs().into_iter().map(|d| d.name).collect();
    assert_eq!(names, ["zeta", "alpha"]);
    assert_eq!(toolbox.defs()[0].description, "z");
}

#[tokio::test]
async fn the_toolbox_calls_a_tool_by_name() {
    let toolbox = Toolbox::new(vec![
        named("first", "f"),
        named("second", "s"),
        Box::new(Add::default()),
    ])
    .unwrap();
    let ctx = ctx();
    assert_eq!(
        toolbox.call("second", r#"{"text": "hi"}"#, &ctx).await,
        Ok("second: hi".to_string())
    );
    assert_eq!(
        toolbox.call("add", r#"{"a": 40, "b": 2}"#, &ctx).await,
        Ok("42".to_string())
    );
    assert_invalid_args(
        toolbox.call("add", r#"{"a": 1}"#, &ctx).await,
        "missing field `b`",
    );
}

#[tokio::test]
async fn an_unknown_tool_is_an_error_for_the_model() {
    let toolbox = Toolbox::new(vec![named("first", "f")]).unwrap();
    assert_eq!(
        toolbox.call("delete_everything", "{}", &ctx()).await,
        Err(ToolError::UnknownTool("delete_everything".into()))
    );
}

#[test]
fn the_toolbox_reports_every_bad_registration() {
    let too_long = "a".repeat(65);
    let error = Toolbox::new(vec![
        named("ok", "fine"),
        named("ok", "again"),
        named("has space", "x"),
        named("", "x"),
        named(&too_long, "x"),
        named("quiet", "   "),
        Box::new(Scalar),
    ])
    .err()
    .unwrap();
    assert_eq!(
        error.problems,
        [
            r#"tool name "ok" is registered twice"#.to_string(),
            r#"tool name "has space" must be 1-64 characters of ASCII letters, digits, `_` or `-`"#
                .to_string(),
            r#"tool name "" must be 1-64 characters of ASCII letters, digits, `_` or `-`"#
                .to_string(),
            format!(
                r#"tool name "{too_long}" must be 1-64 characters of ASCII letters, digits, `_` or `-`"#
            ),
            r#"tool "quiet" has an empty description"#.to_string(),
            r#"tool "scalar" must take a JSON object (its input schema needs "type": "object")"#
                .to_string(),
        ]
    );
}

#[test]
fn every_legal_tool_name_is_accepted() {
    let longest = format!("A-z_09{}", "x".repeat(58));
    assert_eq!(longest.len(), 64);
    let toolbox = Toolbox::new(vec![
        named("a", "x"),
        named("snake_case", "x"),
        named("kebab-case", "x"),
        named("Mixed09", "x"),
        named(&longest, "x"),
    ]);
    assert!(toolbox.is_ok(), "{:?}", toolbox.err());
    // 沒有工具也可以:agent 就只是普通的對話。
    assert_eq!(Toolbox::new(vec![]).unwrap().defs(), []);
}

#[test]
fn the_toolbox_error_lists_each_problem_on_its_own_line() {
    let error = ToolboxError {
        problems: vec!["first".into(), "second".into()],
    };
    assert_eq!(error.to_string(), "invalid tools:\n  - first\n  - second");
}

// ---- 時鐘 ----

#[test]
fn a_fixed_clock_always_answers_the_same() {
    let at: Timestamp = "2026-11-14T02:30:00Z".parse().unwrap();
    assert_eq!(Clock::Fixed(at).now(), at);
}

#[test]
fn the_system_clock_follows_real_time() {
    let before = Timestamp::now();
    let now = Clock::System.now();
    let after = Timestamp::now();
    assert!(
        before <= now && now <= after,
        "{before} <= {now} <= {after}"
    );
}
