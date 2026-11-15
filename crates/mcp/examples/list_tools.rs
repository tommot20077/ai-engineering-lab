//! 連上一個 MCP server,印出它的資訊與工具清單;給了工具名稱與參數就呼叫一次。
//!
//! ```text
//! # stdio:後面接啟動 server 的指令(先編好測試 server)
//! cargo build -p mcp --bin mcp-test-server
//! cargo run -p mcp --example list_tools -- target/debug/mcp-test-server --stdio
//!
//! # 呼叫一個工具:--call <名稱> <JSON 參數>
//! cargo run -p mcp --example list_tools -- --call add '{"a": 2, "b": 3}' target/debug/mcp-test-server --stdio
//!
//! # Streamable HTTP
//! cargo run -p mcp --example list_tools -- --url http://127.0.0.1:3001/mcp
//! ```
//!
//! 子行程只拿到 PATH 與 HOME(跟 agent 一樣不繼承其他環境變數)。

use std::collections::BTreeMap;
use std::process::ExitCode;

use mcp::transport::{
    HttpConfig, HttpLimits, HttpTransport, StdioConfig, StdioLimits, StdioTransport, Transport,
};
use mcp::{ClientOptions, McpClient};
use serde_json::{Map, Value};

const USAGE: &str = "usage: list_tools [--call NAME JSON] (--url URL | COMMAND [ARGS...])";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(1)
        }
    }
}

async fn run(mut args: Vec<String>) -> Result<(), String> {
    let mut call = None;
    if args.first().map(String::as_str) == Some("--call") {
        if args.len() < 3 {
            return Err(USAGE.into());
        }
        let rest = args.split_off(3);
        let json = args.pop().unwrap_or_default();
        let name = args.pop().unwrap_or_default();
        let arguments: Map<String, Value> =
            serde_json::from_str(&json).map_err(|e| format!("--call arguments: {e}"))?;
        call = Some((name, arguments));
        args = rest;
    }
    let transport: Box<dyn Transport> = match args.as_slice() {
        [flag, url] if flag == "--url" => {
            let config = HttpConfig {
                url: url.clone(),
                token: None,
            };
            Box::new(HttpTransport::new(&config, HttpLimits::default()).map_err(|e| e.to_string())?)
        }
        [command, rest @ ..] => {
            let env: BTreeMap<String, String> = ["PATH", "HOME"]
                .iter()
                .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
                .collect();
            let config = StdioConfig {
                command: command.clone(),
                args: rest.to_vec(),
                env,
            };
            Box::new(
                StdioTransport::spawn(&config, StdioLimits::default())
                    .map_err(|e| e.to_string())?,
            )
        }
        [] => return Err(USAGE.into()),
    };

    let client = McpClient::connect(transport, ClientOptions::default())
        .await
        .map_err(|e| e.to_string())?;
    let server = client.server();
    println!(
        "server: {} {}",
        server.name.as_deref().unwrap_or("(no name)"),
        server.version.as_deref().unwrap_or("")
    );
    let list = client.list_tools().await.map_err(|e| e.to_string())?;
    for tool in &list.tools {
        println!(
            "  {:<20} {}",
            tool.name,
            tool.description.as_deref().unwrap_or("")
        );
    }
    for rejected in &list.rejected {
        println!(
            "  (rejected {}: {})",
            rejected.name.as_deref().unwrap_or("?"),
            rejected.reason
        );
    }
    if let Some((name, arguments)) = call {
        let result = client
            .call_tool(&name, arguments)
            .await
            .map_err(|e| e.to_string())?;
        let marker = if result.is_error { " (tool error)" } else { "" };
        println!("{name}{marker}: {}", result.text);
    }
    client.close().await;
    Ok(())
}
