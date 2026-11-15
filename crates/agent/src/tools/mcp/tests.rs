use std::sync::Mutex;

use serde_json::json;

use super::*;
use crate::tool::{Clock, ToolContext};

/// 假的 MCP server:記下收到的呼叫,回傳準備好的結果。
struct Fake {
    calls: Mutex<Vec<(String, Value)>>,
    answer: Result<CallResult, McpError>,
}

impl Fake {
    fn answering(answer: Result<CallResult, McpError>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            answer,
        })
    }
}

impl CallTool for Fake {
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        arguments: Map<String, Value>,
    ) -> BoxFuture<'a, Result<CallResult, McpError>> {
        self.calls
            .lock()
            .unwrap()
            .push((name.to_owned(), Value::Object(arguments)));
        let answer = self.answer.clone();
        Box::pin(async move { answer })
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        run_id: uuid::Uuid::nil(),
        call_id: "call-1".into(),
        clock: Clock::System,
    }
}

fn add_tool() -> mcp::Tool {
    mcp::Tool {
        name: "add".into(),
        title: None,
        description: Some("Adds two integers.".into()),
        input_schema: json!({
            "type": "object",
            "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
            "required": ["a", "b"],
            "additionalProperties": false
        }),
    }
}

fn ok(text: &str) -> Result<CallResult, McpError> {
    Ok(CallResult {
        text: text.into(),
        is_error: false,
    })
}

#[test]
fn the_definition_has_the_server_prefix_and_the_servers_schema() {
    let tool = McpTool::new("test", &add_tool(), Fake::answering(ok("5"))).unwrap();
    let def = tool.def();
    assert_eq!(def.name, "test__add");
    assert_eq!(def.description, "Adds two integers.");
    assert_eq!(def.input_schema, add_tool().input_schema);
}

#[tokio::test]
async fn valid_arguments_go_to_the_server_under_the_original_name() {
    let fake = Fake::answering(ok("5"));
    let tool = McpTool::new("test", &add_tool(), fake.clone()).unwrap();
    assert_eq!(
        tool.call_json(r#"{"a": 2, "b": 3}"#, &ctx()).await,
        Ok("5".into())
    );
    assert_eq!(
        *fake.calls.lock().unwrap(),
        [("add".to_owned(), json!({"a": 2, "b": 3}))]
    );
}

#[tokio::test]
async fn invalid_arguments_never_reach_the_server() {
    // 參數來自模型、不可信:用 server 的 schema 驗證,不合法就交回模型,不送出去(design-part2 §6)。
    let fake = Fake::answering(ok("5"));
    let tool = McpTool::new("test", &add_tool(), fake.clone()).unwrap();
    for (args, expected) in [
        (
            r#"{"a": 2}"#,
            r#"invalid arguments: "b" is a required property"#,
        ),
        (
            r#"{"a": "2", "b": 3}"#,
            r#"invalid arguments: at /a: "2" is not of type "integer""#,
        ),
        (
            r#"[2, 3]"#,
            "invalid arguments: arguments must be a JSON object",
        ),
    ] {
        let err = tool.call_json(args, &ctx()).await.unwrap_err();
        assert_eq!(err.to_string(), expected, "{args}");
    }
    assert!(matches!(
        tool.call_json("not json", &ctx()).await,
        Err(ToolError::InvalidArgs(_))
    ));
    assert_eq!(fake.calls.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn at_most_three_problems_are_reported() {
    // 4 個欄位型別都錯:列 3 個,加上「還有 1 個」。
    let mut tool = add_tool();
    tool.input_schema = json!({"type": "object", "properties": {
        "a": {"type": "integer"}, "b": {"type": "integer"}, "c": {"type": "integer"}, "d": {"type": "integer"}
    }});
    let tool = McpTool::new("test", &tool, Fake::answering(ok(""))).unwrap();
    let err = tool
        .call_json(r#"{"a": "w", "b": "x", "c": "y", "d": "z"}"#, &ctx())
        .await
        .unwrap_err();
    let ToolError::InvalidArgs(message) = err else {
        panic!("{err:?}")
    };
    assert_eq!(message.matches(" is not of type ").count(), 3, "{message}");
    assert!(message.ends_with("; (and 1 more)"), "{message}");
}

#[tokio::test]
async fn tool_errors_and_protocol_errors_are_failures_for_the_model() {
    let failed = Fake::answering(Ok(CallResult {
        text: "it broke".into(),
        is_error: true,
    }));
    let tool = McpTool::new("test", &add_tool(), failed).unwrap();
    assert_eq!(
        tool.call_json(r#"{"a": 1, "b": 2}"#, &ctx()).await,
        Err(ToolError::Failed("it broke".into()))
    );
    let timeout = Fake::answering(Err(McpError::Timeout {
        method: "tools/call".into(),
    }));
    let tool = McpTool::new("test", &add_tool(), timeout).unwrap();
    assert_eq!(
        tool.call_json(r#"{"a": 1, "b": 2}"#, &ctx()).await,
        Err(ToolError::Failed(
            "the MCP server did not answer `tools/call` in time".into()
        ))
    );
}

#[test]
fn descriptions_are_cleaned_and_capped() {
    // server 給的說明會進 prompt:控制字元(包括換行)換成空白、最多 1024 個字元。
    let mut tool = add_tool();
    tool.description = Some("line one\nline two\u{1b}[31m\tend".into());
    let def = McpTool::new("s", &tool, Fake::answering(ok("")))
        .unwrap()
        .def();
    assert_eq!(def.description, "line one line two [31m end");

    tool.description = Some("說".repeat(2000));
    let def = McpTool::new("s", &tool, Fake::answering(ok("")))
        .unwrap()
        .def();
    assert_eq!(def.description.chars().count(), MAX_DESCRIPTION_CHARS);
    assert!(def.description.ends_with('…'));

    tool.description = Some("說".repeat(MAX_DESCRIPTION_CHARS));
    let def = McpTool::new("s", &tool, Fake::answering(ok("")))
        .unwrap()
        .def();
    assert_eq!(def.description, "說".repeat(MAX_DESCRIPTION_CHARS));
}

#[test]
fn a_missing_description_gets_a_neutral_one() {
    // Toolbox 不收空的說明;沒給就寫「哪個 server 的哪個工具」,不用 server 給的 title(一樣不可信,而且沒有比較多資訊)。
    for description in [None, Some("   ".to_owned()), Some("\n\u{7}".to_owned())] {
        let mut tool = add_tool();
        tool.description = description;
        let def = McpTool::new("notes", &tool, Fake::answering(ok("")))
            .unwrap()
            .def();
        assert_eq!(def.description, "The tool `add` of the MCP server `notes`.");
    }
}

#[test]
fn a_schema_that_does_not_compile_is_refused() {
    let mut tool = add_tool();
    tool.input_schema = json!({"type": "object", "properties": {"a": {"type": "not-a-type"}}});
    let err = McpTool::new("s", &tool, Fake::answering(ok("")))
        .err()
        .unwrap();
    assert!(err.starts_with("has an invalid input schema: "), "{err}");
}

#[tokio::test]
async fn patterns_use_the_linear_time_engine() {
    // `(a+)+$` 在會回溯的引擎上,遇到一長串 a 加一個 b 會跑到天荒地老(ReDoS);`regex` crate 保證線性時間。
    let mut tool = add_tool();
    tool.input_schema =
        json!({"type": "object", "properties": {"s": {"type": "string", "pattern": "^(a+)+$"}}});
    let tool = McpTool::new("s", &tool, Fake::answering(ok("fine"))).unwrap();
    let evil = format!(r#"{{"s": "{}b"}}"#, "a".repeat(100_000));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tool.call_json(&evil, &ctx()),
    )
    .await
    .expect("pattern matching must not hang");
    assert!(matches!(result, Err(ToolError::InvalidArgs(_))));
    // 不支援的語法(lookahead)在啟動時就被拒絕,不會在執行時才出事。
    let mut lookahead = add_tool();
    lookahead.input_schema =
        json!({"type": "object", "properties": {"s": {"type": "string", "pattern": "^(?=a)"}}});
    assert!(McpTool::new("s", &lookahead, Fake::answering(ok(""))).is_err());
}

#[test]
fn external_references_are_not_fetched() {
    // 規格:`$ref` 指向外部 URI 時預設不能去抓。關掉 jsonschema 的預設功能後,這種 schema 編譯失敗(工具不提供)。
    let mut tool = add_tool();
    tool.input_schema =
        json!({"type": "object", "properties": {"a": {"$ref": "https://example.com/schema.json"}}});
    assert!(McpTool::new("s", &tool, Fake::answering(ok(""))).is_err());
}

fn list(names: &[&str]) -> ToolList {
    ToolList {
        tools: names
            .iter()
            .map(|n| mcp::Tool {
                name: (*n).into(),
                ..add_tool()
            })
            .collect(),
        rejected: vec![mcp::RejectedTool {
            name: Some("broken".into()),
            reason: "`name` must be 1 to 128 of A-Z a-z 0-9 _ - .".into(),
        }],
    }
}

fn names(tools: &[McpTool]) -> Vec<String> {
    tools.iter().map(|t| t.def().name).collect()
}

#[test]
fn only_allowed_tools_are_offered_in_the_allowlist_order() {
    // 白名單外的工具不提供,也不會出現在 prompt 裡(D-050)。
    let selected = select(
        "test",
        &list(&["echo", "add", "secret_admin"]),
        &["add".into(), "echo".into()],
        Fake::answering(ok("")),
    )
    .unwrap();
    assert_eq!(names(&selected), ["test__add", "test__echo"]);
}

#[test]
fn an_allowed_tool_the_server_lacks_is_a_startup_error() {
    // design-part2 §9-3:設定跟實際不一致要大聲說。被拒絕的工具說出原因。所有問題一次列出。
    let problems = select(
        "test",
        &list(&["echo"]),
        &["echo".into(), "add".into(), "broken".into()],
        Fake::answering(ok("")),
    )
    .err()
    .unwrap();
    assert_eq!(
        problems,
        [
            "MCP server `test` does not offer the tool `add`".to_owned(),
            "MCP server `test` offers the tool `broken`, but it was rejected: `name` must be 1 to 128 of A-Z a-z 0-9 _ - ."
                .to_owned(),
        ]
    );
}

#[test]
fn a_tool_whose_schema_fails_is_a_startup_error() {
    let mut tools = list(&["add"]);
    if let Some(tool) = tools.tools.first_mut() {
        tool.input_schema = json!({"type": "object", "properties": {"a": {"type": 5}}});
    }
    let problems = select("test", &tools, &["add".into()], Fake::answering(ok("")))
        .err()
        .unwrap();
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].starts_with("MCP server `test`: the tool `add` has an invalid input schema: "),
        "{problems:?}"
    );
}
