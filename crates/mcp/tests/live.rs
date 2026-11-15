//! 對官方的參考 server(`@modelcontextprotocol/server-everything`)跑一次(design-part2 §7、D-051)。
//! 要 Node(`npx`)與網路,所以是 `#[ignore]`,手動執行:
//!
//! ```text
//! # stdio:測試自己用 npx 啟動 server
//! cargo test -p mcp --test live -- --ignored live_stdio
//!
//! # Streamable HTTP:先在另一個終端機啟動 server
//! PORT=3001 npx -y @modelcontextprotocol/server-everything@2.3.0 streamableHttp
//! cargo test -p mcp --test live -- --ignored live_http
//! ```
//!
//! 可以用環境變數換掉預設值:`MCP_LIVE_COMMAND`(stdio 的完整指令,空白分隔)、`MCP_LIVE_URL`(HTTP 的 endpoint)。
//! 測試 server 是我們照規格自己寫的;這裡確認「照規格寫的」跟「別人照規格寫的」真的對得上(第一部分的教訓:假的要跟真的對過)。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::time::Duration;

use mcp::transport::{
    HttpConfig, HttpLimits, HttpTransport, StdioConfig, StdioLimits, StdioTransport, Transport,
};
use mcp::{ClientOptions, McpClient};
use serde_json::{Map, Value, json};

const DEFAULT_COMMAND: &str = "npx -y @modelcontextprotocol/server-everything@2.3.0 stdio";
const DEFAULT_URL: &str = "http://127.0.0.1:3001/mcp";

fn args(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

async fn check(transport: Box<dyn Transport>) {
    let options = ClientOptions {
        // npx 第一次要下載套件,給久一點。
        request_timeout: Duration::from_secs(120),
        ..ClientOptions::default()
    };
    let client = McpClient::connect(transport, options).await.unwrap();
    eprintln!("server: {:?}", client.server());

    let list = client.list_tools().await.unwrap();
    let names: Vec<&str> = list.tools.iter().map(|t| t.name.as_str()).collect();
    eprintln!("tools ({}): {names:?}", names.len());
    // 我們的檢查比規格的 SHOULD 嚴格(名稱、x-mcp-header):參考 server 的工具一個都不該被拒絕。
    assert_eq!(list.rejected, [], "tools we rejected");
    for wanted in ["echo", "get-sum", "get-header-params"] {
        assert!(names.contains(&wanted), "missing {wanted}");
    }

    let echo = client
        .call_tool("echo", args(json!({"message": "hello 你好"})))
        .await
        .unwrap();
    assert_eq!(echo.text, "Echo: hello 你好");
    assert!(!echo.is_error);

    let sum = client
        .call_tool("get-sum", args(json!({"a": 2, "b": 3})))
        .await
        .unwrap();
    assert!(sum.text.contains('5'), "{sum:?}");

    // `region` 標了 `x-mcp-header: "Region"`:HTTP 時 server 檢查 `Mcp-Param-Region` 跟參數一致,不一致回 -32020。
    // 中文要 base64 編碼,剛好測到編碼。stdio 沒有 header,server 回報 null。
    let header = client
        .call_tool("get-header-params", args(json!({"region": "日本"})))
        .await
        .unwrap();
    eprintln!("get-header-params: {}", header.text);
    assert!(!header.is_error, "{header:?}");

    // 不存在的工具:協定錯誤(不是工具錯誤)。
    assert!(client.call_tool("no-such-tool", Map::new()).await.is_err());
    client.close().await;
}

#[tokio::test]
#[ignore = "needs Node (npx) and network access"]
async fn live_stdio_reference_server() {
    let line = std::env::var("MCP_LIVE_COMMAND").unwrap_or_else(|_| DEFAULT_COMMAND.into());
    let mut words = line.split_whitespace().map(String::from);
    let command = words.next().unwrap();
    // 測試(不是 library)可以讀環境變數:npx 要 PATH 找 node、HOME 放快取。
    let env: BTreeMap<String, String> = ["PATH", "HOME"]
        .iter()
        .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
        .collect();
    let config = StdioConfig {
        command,
        args: words.collect(),
        env,
    };
    check(Box::new(
        StdioTransport::spawn(&config, StdioLimits::default()).unwrap(),
    ))
    .await;
}

#[tokio::test]
#[ignore = "needs a reference server started with `streamableHttp`"]
async fn live_http_reference_server() {
    let url = std::env::var("MCP_LIVE_URL").unwrap_or_else(|_| DEFAULT_URL.into());
    let config = HttpConfig { url, token: None };
    check(Box::new(
        HttpTransport::new(&config, HttpLimits::default()).unwrap(),
    ))
    .await;
}
