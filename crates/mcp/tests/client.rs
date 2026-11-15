//! `McpClient` 對測試 server 的整合測試(design-part2 §4,協定 2026-07-28)。
//!
//! 每個測試都對兩種傳輸(stdio、Streamable HTTP)各跑一次:同一個 server 程式、同樣的旗標、同樣的預期結果
//! (design-part2 §7)。失敗時 stderr 印出是哪一種傳輸。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use common::{KINDS, connect, connect_with, within};
use mcp::jsonrpc::RpcError;
use mcp::{CallResult, ClientOptions, McpClient, McpError, RejectedTool, ServerInfo};
use serde_json::{Map, Value, json};

fn args(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

async fn call(client: &McpClient, name: &str, arguments: Value) -> Result<CallResult, McpError> {
    within(client.call_tool(name, args(arguments))).await
}

fn text(t: &str) -> Result<CallResult, McpError> {
    Ok(CallResult {
        text: t.into(),
        is_error: false,
    })
}

// ---- 連線(server/discover)----

#[tokio::test]
async fn connect_discovers_the_server() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        let client = connect(kind).await;
        assert_eq!(
            client.server(),
            &ServerInfo {
                name: Some("mcp-test-server".into()),
                version: Some("0.1.0".into()),
                instructions: Some("A server for testing MCP clients.".into()),
            }
        );
        client.close().await;
    }
}

#[tokio::test]
async fn every_request_carries_the_version_capabilities_and_client_info() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        // 沒有 initialize:server 不記得前一個請求,每個請求都要自己帶(規格 basic §_meta)。
        let options = ClientOptions {
            name: "lab-test".into(),
            version: "9.9.9".into(),
            ..ClientOptions::default()
        };
        let client = connect_with(kind, &[], options).await.unwrap();
        let meta: Value =
            serde_json::from_str(&call(&client, "meta", json!({})).await.unwrap().text).unwrap();
        assert_eq!(
            meta,
            json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": {"name": "lab-test", "version": "9.9.9"}
            })
        );
    }
}

#[tokio::test]
async fn the_default_options() {
    let options = ClientOptions::default();
    assert_eq!(options.name, "ai-engineering-lab");
    assert_eq!(options.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(options.request_timeout, Duration::from_secs(30));
    assert_eq!(options.max_pages, 100);
    assert_eq!(options.max_tools, 1000);
}

#[tokio::test]
async fn a_server_without_our_version_is_refused() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        // server 回 -32022,附上它支援的版本。
        let err = connect_with(
            kind,
            &["--versions", "2025-06-18,2025-11-25"],
            ClientOptions::default(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            err,
            McpError::UnsupportedVersion {
                supported: vec!["2025-06-18".into(), "2025-11-25".into()]
            }
        );
    }
}

#[tokio::test]
async fn a_legacy_server_is_detected_by_the_discover_probe() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        // 舊版 server 不認得 server/discover(-32601)。不能接著送 tools/call:有些舊版 server 會用舊的語意處理它。
        let err = connect_with(kind, &["--legacy"], ClientOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(
            err,
            McpError::NotModern(
                "the MCP server returned an error: JSON-RPC error -32601: Method not found".into()
            )
        );
    }
}

#[tokio::test]
async fn a_server_that_never_answers_discover_is_not_modern() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        let options = ClientOptions {
            request_timeout: Duration::from_millis(300),
            ..ClientOptions::default()
        };
        let err = connect_with(kind, &["--silent-discover"], options)
            .await
            .err()
            .unwrap();
        assert_eq!(
            err,
            McpError::NotModern("no answer to `server/discover` within 300ms".into())
        );
    }
}

#[tokio::test]
async fn a_server_without_tools_is_refused() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        let err = connect_with(kind, &["--no-tools"], ClientOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(err, McpError::NoTools);
    }
}

// ---- tools/list ----

#[tokio::test]
async fn list_tools_follows_every_page() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        // 12 個工具、每頁 4 個:要翻 3 頁(最後一頁剛好滿,沒有 nextCursor)。
        let client = connect(kind).await;
        let list = within(client.list_tools()).await.unwrap();
        let names: Vec<&str> = list.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "echo",
                "add",
                "slow",
                "big",
                "fail",
                "mixed",
                "structured",
                "meta",
                "ask",
                "mystery",
                "plain",
                "region"
            ]
        );
        assert_eq!(list.rejected, []);
        let echo = &list.tools[0];
        assert_eq!(echo.title.as_deref(), Some("Echo"));
        assert_eq!(
            echo.description.as_deref(),
            Some("Returns the text it is given.")
        );
        assert_eq!(
            echo.input_schema,
            json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
        );
        // 沒有說明的工具。
        assert_eq!(list.tools[10].description, None);
    }
}

#[tokio::test]
async fn malformed_tools_are_rejected_one_by_one() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        // 一個壞掉的工具不讓整份清單失效;被拒絕的列出來(白名單裡的工具被拒絕時,agent 啟動會失敗)。
        let client = connect_with(kind, &["--bad-tools"], ClientOptions::default())
            .await
            .unwrap();
        let list = within(client.list_tools()).await.unwrap();
        assert_eq!(list.tools.len(), 12);
        assert_eq!(
            list.rejected,
            [
                RejectedTool {
                    name: None,
                    reason: "`name` must be a string".into()
                },
                RejectedTool {
                    name: Some("no_schema".into()),
                    reason: "`inputSchema` must be an object whose `type` is \"object\"".into()
                },
                RejectedTool {
                    name: Some("array_schema".into()),
                    reason: "`inputSchema` must be an object whose `type` is \"object\"".into()
                },
                RejectedTool {
                    name: None,
                    reason: "a tool must be a JSON object".into()
                },
                RejectedTool {
                    name: Some("bad_header".into()),
                    reason: "`x-mcp-header` `N` must be on a string, integer or boolean parameter"
                        .into()
                },
            ]
        );
    }
}

#[tokio::test]
async fn a_repeated_cursor_stops_the_listing() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        // server 的 nextCursor 一直是同一個:照著翻會永遠翻不完。
        let client = connect_with(kind, &["--loop-cursor"], ClientOptions::default())
            .await
            .unwrap();
        assert_eq!(
            within(client.list_tools()).await,
            Err(McpError::InvalidResult {
                method: "tools/list".into(),
                reason: "the server repeated a cursor".into()
            })
        );
    }
}

#[tokio::test]
async fn the_listing_has_a_page_and_a_tool_limit() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        let options = ClientOptions {
            max_pages: 2,
            ..ClientOptions::default()
        };
        let client = connect_with(kind, &[], options).await.unwrap();
        assert_eq!(
            within(client.list_tools()).await,
            Err(McpError::InvalidResult {
                method: "tools/list".into(),
                reason: "more than 2 pages".into()
            })
        );
        // 剛好 3 頁:可以。
        let options = ClientOptions {
            max_pages: 3,
            max_tools: 12,
            ..ClientOptions::default()
        };
        let client = connect_with(kind, &[], options).await.unwrap();
        assert_eq!(within(client.list_tools()).await.unwrap().tools.len(), 12);

        let options = ClientOptions {
            max_tools: 11,
            ..ClientOptions::default()
        };
        let client = connect_with(kind, &[], options).await.unwrap();
        assert_eq!(
            within(client.list_tools()).await,
            Err(McpError::InvalidResult {
                method: "tools/list".into(),
                reason: "more than 11 tools".into()
            })
        );
    }
}

// ---- tools/call ----

#[tokio::test]
async fn call_tool_returns_the_text() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        let client = connect(kind).await;
        assert_eq!(
            call(&client, "echo", json!({"text": "hi"})).await,
            text("hi")
        );
        assert_eq!(
            call(&client, "add", json!({"a": 2, "b": 3})).await,
            text("5")
        );
    }
}

#[tokio::test]
async fn a_tool_error_is_a_result_not_an_error() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        // isError:工具自己失敗,交回模型讓它修正(design-part2 §4)。
        let client = connect(kind).await;
        assert_eq!(
            call(&client, "fail", json!({})).await,
            Ok(CallResult {
                text: "it broke".into(),
                is_error: true
            })
        );
    }
}

#[tokio::test]
async fn content_that_is_not_text_becomes_a_one_line_note() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        // §9-5:圖片、音訊、資源換成一行說明;不認得的內容也是。文字照順序,一段一行。
        let client = connect(kind).await;
        assert_eq!(
            call(&client, "mixed", json!({})).await,
            text(
                "before\n[image/png omitted]\n[audio/wav omitted]\n[resource_link omitted]\n\
                 [resource omitted]\n[unsupported content omitted]\nafter"
            )
        );
    }
}

#[tokio::test]
async fn structured_content_is_used_when_there_is_no_text() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        let client = connect(kind).await;
        assert_eq!(
            call(&client, "structured", json!({})).await,
            text(r#"{"answer":42}"#)
        );
    }
}

#[tokio::test]
async fn result_types() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        let client = connect(kind).await;
        // 沒有 resultType(舊版 server 的格式):當成 complete。
        assert_eq!(
            call(&client, "plain", json!({})).await,
            text("no resultType")
        );
        // input_required(MRTR):不支援。
        assert_eq!(
            call(&client, "ask", json!({})).await,
            Err(McpError::InputRequired)
        );
        // 不認得的 resultType:規格說一律當成不合法。
        assert_eq!(
            call(&client, "mystery", json!({})).await,
            Err(McpError::InvalidResult {
                method: "tools/call".into(),
                reason: "unknown resultType".into()
            })
        );
    }
}

#[tokio::test]
async fn an_unknown_tool_is_an_rpc_error() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        let client = connect(kind).await;
        assert_eq!(
            call(&client, "nope", json!({})).await,
            Err(McpError::Rpc(RpcError {
                code: -32602,
                message: "Unknown tool: nope".into(),
                data: None
            }))
        );
    }
}

#[tokio::test]
async fn a_slow_call_times_out_and_the_connection_stays_usable() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        let options = ClientOptions {
            request_timeout: Duration::from_millis(200),
            ..ClientOptions::default()
        };
        let client = connect_with(kind, &[], options).await.unwrap();
        assert_eq!(
            call(&client, "slow", json!({"ms": 2000})).await,
            Err(McpError::Timeout {
                method: "tools/call".into()
            })
        );
        assert_eq!(
            call(&client, "echo", json!({"text": "still here"})).await,
            text("still here")
        );
    }
}

#[tokio::test]
async fn header_parameters_reach_the_server() {
    for kind in KINDS {
        eprintln!("transport: {kind:?}");
        // HTTP:server 檢查 `Mcp-Param-Region` 跟參數一致(中文要 base64 編碼),不一致回 400 + -32020。
        // stdio 沒有 header,結果一樣。
        let client = connect(kind).await;
        within(client.list_tools()).await.unwrap();
        for region in ["us-west1", "日本", " padded "] {
            assert_eq!(
                call(&client, "region", json!({"region": region})).await,
                text(&format!("region={region}"))
            );
        }
    }
}
