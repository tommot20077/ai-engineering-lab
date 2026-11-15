use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::json;

use super::*;
use crate::jsonrpc::RpcError;

/// 假的傳輸:照順序交出準備好的回應,記下送出的請求、有沒有被關閉。
/// 整合測試用真的 server;這裡用來製造測試 server 做不出來的回應(discover 少欄位之類)。
#[derive(Default)]
struct Fake {
    responses: Mutex<VecDeque<Result<Value, McpError>>>,
    sent: Arc<Mutex<Vec<(String, Value)>>>,
    closed: Arc<AtomicBool>,
}

impl Fake {
    fn new(responses: Vec<Result<Value, McpError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            ..Self::default()
        }
    }
}

impl Transport for Fake {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        self.sent
            .lock()
            .unwrap()
            .push((method.to_owned(), params.unwrap_or(Value::Null)));
        let next = self.responses.lock().unwrap().pop_front();
        Box::pin(async move {
            next.unwrap_or(Err(McpError::Closed {
                stderr: String::new(),
            }))
        })
    }

    fn notify<'a>(&'a self, _: &'a str, _: Option<Value>) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async { Ok(()) })
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        self.closed.store(true, Ordering::SeqCst);
        Box::pin(async {})
    }
}

fn discover(result: Value) -> Result<Value, McpError> {
    Ok(result)
}

async fn connect_fake(
    responses: Vec<Result<Value, McpError>>,
) -> (Result<McpClient, McpError>, Arc<AtomicBool>) {
    let fake = Fake::new(responses);
    let closed = Arc::clone(&fake.closed);
    let result = McpClient::connect(Box::new(fake), ClientOptions::default()).await;
    (result, closed)
}

#[tokio::test]
async fn discover_without_our_version_is_unsupported_and_closes_the_transport() {
    let (result, closed) = connect_fake(vec![discover(json!({
        "resultType": "complete", "supportedVersions": ["2025-11-25"], "capabilities": {"tools": {}}
    }))])
    .await;
    assert_eq!(
        result.err(),
        Some(McpError::UnsupportedVersion {
            supported: vec!["2025-11-25".into()]
        })
    );
    // 連線失敗時要關掉傳輸(子行程才會結束),不能只是丟掉。
    assert!(closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_successful_connect_does_not_close_the_transport() {
    let (result, closed) = connect_fake(vec![discover(json!({
        "supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}
    }))])
    .await;
    assert!(result.is_ok());
    assert!(!closed.load(Ordering::SeqCst));
    // 沒有 serverInfo、instructions 也可以。
    assert_eq!(result.unwrap().server(), &ServerInfo::default());
}

#[tokio::test]
async fn a_discover_result_without_supported_versions_is_invalid() {
    for result in [
        json!({"capabilities": {"tools": {}}}),
        json!({"supportedVersions": "2026-07-28", "capabilities": {"tools": {}}}),
    ] {
        let (result, _) = connect_fake(vec![discover(result)]).await;
        assert_eq!(
            result.err(),
            Some(McpError::InvalidResult {
                method: "server/discover".into(),
                reason: "`supportedVersions` must be an array of strings".into()
            })
        );
    }
}

#[tokio::test]
async fn tools_must_be_an_object_in_the_capabilities() {
    for capabilities in [json!({}), json!({"tools": true}), json!(null)] {
        let (result, _) = connect_fake(vec![discover(json!({
            "supportedVersions": ["2026-07-28"], "capabilities": capabilities
        }))])
        .await;
        assert_eq!(result.err(), Some(McpError::NoTools), "{capabilities}");
    }
}

#[tokio::test]
async fn modern_errors_from_discover_are_not_mistaken_for_a_legacy_server() {
    // -32020 ~ -32022 是 2026-07-28 定義的錯誤:對方是新版 server,原樣回報,不說「可能是舊版」。
    let error = RpcError {
        code: -32021,
        message: "missing capability".into(),
        data: None,
    };
    let (result, _) = connect_fake(vec![Err(McpError::Rpc(error.clone()))]).await;
    assert_eq!(result.err(), Some(McpError::Rpc(error)));
    // 其他錯誤碼(-32601、-32602……)就是「聽不懂」。
    let (result, _) = connect_fake(vec![Err(McpError::Rpc(RpcError {
        code: -32019,
        message: "x".into(),
        data: None,
    }))])
    .await;
    assert!(matches!(result.err(), Some(McpError::NotModern(_))));
}

#[test]
fn the_versions_in_an_unsupported_version_error_are_made_printable() {
    // server 給的字串會印到終端機:只留可印的字元、每個最多 32 字、最多 10 個;不是字串的略過。
    let data = json!({"supported": [
        "2025-11-25", 7, "evil\u{1b}[31m", "x".repeat(40),
        "a", "b", "c", "d", "e", "f", "g", "h", "i"
    ]});
    assert_eq!(
        unsupported_versions(Some(&data)),
        [
            "2025-11-25".to_owned(),
            "evil?[31m".into(),
            "x".repeat(32),
            "a".into(),
            "b".into(),
            "c".into(),
            "d".into(),
            "e".into(),
            "f".into(),
            "g".into(),
        ]
    );
    assert_eq!(unsupported_versions(None), Vec::<String>::new());
}

#[test]
fn a_32022_anywhere_becomes_unsupported_version() {
    let error = McpError::Rpc(RpcError {
        code: UNSUPPORTED_PROTOCOL_VERSION,
        message: "Unsupported protocol version".into(),
        data: Some(json!({"supported": ["2025-11-25"], "requested": "2026-07-28"})),
    });
    assert_eq!(
        classify(error),
        McpError::UnsupportedVersion {
            supported: vec!["2025-11-25".into()]
        }
    );
    let other = McpError::Rpc(RpcError {
        code: -32602,
        message: "x".into(),
        data: None,
    });
    assert_eq!(classify(other.clone()), other);
}

#[test]
fn tool_names_follow_the_spec() {
    // 1~128 個字元;只有英數字、`_`、`-`、`.`(規格 server/tools「Tool Names」)。
    let tool = |name: &str| parse_tool(json!({"name": name, "inputSchema": {"type": "object"}}));
    for good in [
        "getUser",
        "DATA_EXPORT_v2",
        "admin.tools.list",
        "a-b",
        &"x".repeat(128),
    ] {
        assert!(tool(good).is_ok(), "{good}");
    }
    for bad in ["", "a b", "a,b", "名字", "a/b", &"x".repeat(129)] {
        assert_eq!(
            tool(bad).err().map(|r| r.reason),
            Some("`name` must be 1 to 128 of A-Z a-z 0-9 _ - .".to_owned()),
            "{bad}"
        );
    }
}

#[test]
fn a_rejected_name_is_made_printable() {
    let rejected = parse_tool(json!({"name": "bad\u{7}name with spaces", "inputSchema": {}}))
        .err()
        .unwrap();
    assert_eq!(rejected.name.as_deref(), Some("bad?name with spaces"));
}

#[test]
fn optional_tool_fields_that_are_not_strings_are_dropped() {
    let tool = parse_tool(json!({
        "name": "t", "title": 5, "description": ["x"], "inputSchema": {"type": "object"}
    }))
    .unwrap()
    .0;
    assert_eq!(tool.title, None);
    assert_eq!(tool.description, None);
}

#[test]
fn media_types_in_notes_are_checked() {
    // mimeType 是 server 給的:不像 media type 的(含控制字元、空白)不放進說明。
    assert_eq!(
        content_text(
            &[json!({"type": "image", "mimeType": "image/svg+xml"})],
            None
        ),
        "[image/svg+xml omitted]"
    );
    assert_eq!(
        content_text(
            &[json!({"type": "image", "mimeType": "image/png\u{1b}[2J"})],
            None
        ),
        "[image omitted]"
    );
    assert_eq!(
        content_text(&[json!({"type": "audio", "mimeType": "no slash"})], None),
        "[audio omitted]"
    );
    assert_eq!(
        content_text(&[json!({"type": "audio"})], None),
        "[audio omitted]"
    );
}

#[test]
fn malformed_text_blocks_are_unsupported_content() {
    assert_eq!(
        content_text(
            &[
                json!({"type": "text"}),
                json!("loose string"),
                json!({"text": "no type"})
            ],
            None
        ),
        "[unsupported content omitted]\n[unsupported content omitted]\n[unsupported content omitted]"
    );
}

#[test]
fn structured_content_only_fills_in_when_there_is_no_text() {
    let structured = json!({"a": 1});
    assert_eq!(content_text(&[], Some(&structured)), r#"{"a":1}"#);
    assert_eq!(
        content_text(
            &[json!({"type": "text", "text": "words"})],
            Some(&structured)
        ),
        "words"
    );
    // 只有圖片:沒有文字,用 structuredContent。
    assert_eq!(
        content_text(
            &[json!({"type": "image", "mimeType": "image/png"})],
            Some(&structured)
        ),
        "[image/png omitted]\n{\"a\":1}"
    );
    assert_eq!(content_text(&[], None), "");
}

#[tokio::test]
async fn call_results_are_checked() {
    let ok_discover =
        discover(json!({"supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}}));
    let (client, _) = connect_fake(vec![
        ok_discover,
        Ok(json!({"content": "not an array"})),
        Ok(json!({"content": [], "isError": "yes"})),
        Ok(json!([1, 2])),
        Ok(json!({"resultType": 7, "content": []})),
    ])
    .await;
    let client = client.unwrap();
    let invalid = |reason: &str| {
        Err(McpError::InvalidResult {
            method: "tools/call".into(),
            reason: reason.into(),
        })
    };
    assert_eq!(
        client.call_tool("t", Map::new()).await,
        invalid("`content` must be an array")
    );
    assert_eq!(
        client.call_tool("t", Map::new()).await,
        invalid("`isError` must be a boolean")
    );
    assert_eq!(
        client.call_tool("t", Map::new()).await,
        invalid("the result is not a JSON object")
    );
    assert_eq!(
        client.call_tool("t", Map::new()).await,
        invalid("unknown resultType")
    );
}

#[tokio::test]
async fn call_tool_sends_the_name_and_arguments() {
    let fake = Fake::new(vec![
        discover(json!({"supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}})),
        Ok(json!({"content": []})),
    ]);
    let sent = Arc::clone(&fake.sent);
    let client = McpClient::connect(Box::new(fake), ClientOptions::default())
        .await
        .unwrap();
    let mut arguments = Map::new();
    arguments.insert("x".into(), json!(1));
    client.call_tool("calc", arguments).await.unwrap();
    let sent = sent.lock().unwrap();
    let (method, params) = &sent[1];
    assert_eq!(method, "tools/call");
    assert_eq!(params["name"], "calc");
    assert_eq!(params["arguments"], json!({"x": 1}));
    assert_eq!(
        params["_meta"]["io.modelcontextprotocol/protocolVersion"],
        "2026-07-28"
    );
}

#[tokio::test]
async fn list_results_are_checked() {
    let ok_discover =
        discover(json!({"supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}}));
    let tool = |name: &str| json!({"name": name, "inputSchema": {"type": "object"}});
    let (client, _) = connect_fake(vec![
        ok_discover,
        // 名稱重複:保留第一個,第二個列進 rejected。
        Ok(json!({"tools": [tool("a"), tool("b"), tool("a")]})),
        Ok(json!({"tools": {"a": 1}})),
        Ok(json!({"tools": [], "nextCursor": 3})),
    ])
    .await;
    let client = client.unwrap();
    let list = client.list_tools().await.unwrap();
    let names: Vec<&str> = list.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    assert_eq!(
        list.rejected,
        [RejectedTool {
            name: Some("a".into()),
            reason: "duplicate name".into()
        }]
    );
    let invalid = |reason: &str| {
        Err(McpError::InvalidResult {
            method: "tools/list".into(),
            reason: reason.into(),
        })
    };
    assert_eq!(
        client.list_tools().await,
        invalid("`tools` must be an array")
    );
    assert_eq!(
        client.list_tools().await,
        invalid("`nextCursor` must be a string")
    );
}

#[test]
fn a_tool_with_an_invalid_header_annotation_is_rejected() {
    let rejected = parse_tool(json!({"name": "t", "inputSchema": {
        "type": "object", "properties": {"n": {"type": "number", "x-mcp-header": "N"}}
    }}))
    .err()
    .unwrap();
    assert_eq!(
        rejected.reason,
        "`x-mcp-header` `N` must be on a string, integer or boolean parameter"
    );
}

type Headers = Vec<(String, String)>;

#[tokio::test]
async fn call_tool_sends_the_header_values_of_listed_tools() {
    // 假傳輸記下 `request_with_headers` 收到的 header。預設實作會丟掉 header,所以這裡要自己實作。
    struct Recording {
        inner: Fake,
        headers: Arc<Mutex<Vec<Headers>>>,
    }
    impl Transport for Recording {
        fn request<'a>(
            &'a self,
            m: &'a str,
            p: Option<Value>,
        ) -> BoxFuture<'a, Result<Value, McpError>> {
            self.inner.request(m, p)
        }
        fn request_with_headers<'a>(
            &'a self,
            m: &'a str,
            p: Option<Value>,
            h: Vec<(String, String)>,
        ) -> BoxFuture<'a, Result<Value, McpError>> {
            self.headers.lock().unwrap().push(h);
            self.inner.request(m, p)
        }
        fn notify<'a>(
            &'a self,
            m: &'a str,
            p: Option<Value>,
        ) -> BoxFuture<'a, Result<(), McpError>> {
            self.inner.notify(m, p)
        }
        fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
            Box::new(self.inner).close()
        }
    }
    let headers = Arc::new(Mutex::new(Vec::new()));
    let transport = Recording {
        inner: Fake::new(vec![
            discover(json!({"supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}})),
            Ok(
                json!({"tools": [{"name": "sql", "inputSchema": {"type": "object", "properties": {
                    "region": {"type": "string", "x-mcp-header": "Region"}
                }}}]}),
            ),
            Ok(json!({"content": []})),
            Ok(json!({"content": []})),
        ]),
        headers: Arc::clone(&headers),
    };
    let client = McpClient::connect(Box::new(transport), ClientOptions::default())
        .await
        .unwrap();
    client.list_tools().await.unwrap();
    let mut arguments = Map::new();
    arguments.insert("region".into(), json!("日本"));
    client.call_tool("sql", arguments.clone()).await.unwrap();
    // 沒列過的工具:沒有 header。
    client.call_tool("other", arguments).await.unwrap();
    let headers = headers.lock().unwrap();
    // discover、tools/list、兩次 tools/call。
    assert_eq!(headers.len(), 4);
    assert_eq!(
        headers[2],
        [("Region".to_owned(), "=?base64?5pel5pys?=".to_owned())]
    );
    assert_eq!(headers[3], []);
}
