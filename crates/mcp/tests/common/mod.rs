//! 整合測試共用:啟動測試 server、兩種傳輸。
//!
//! `tests/` 底下每個 `.rs` 是獨立的測試程式;共用的程式放 `tests/common/mod.rs`(放 `tests/common.rs` 會被當成另一個測試程式)。

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::ops::Deref;
use std::process::Stdio;
use std::time::Duration;

use mcp::transport::{
    HttpConfig, HttpLimits, HttpTransport, StdioConfig, StdioLimits, StdioTransport, Transport,
};
use mcp::{ClientOptions, McpClient, McpError};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

pub const SERVER: &str = env!("CARGO_BIN_EXE_mcp-test-server");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Stdio,
    Http,
}

pub const KINDS: [Kind; 2] = [Kind::Stdio, Kind::Http];

pub async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("timed out")
}

/// 啟動 HTTP 模式的測試 server,回傳它印出的 URL 與子行程(`kill_on_drop`:測試結束就收掉)。
pub async fn http_server(flags: &[&str]) -> (String, Child) {
    let mut child = Command::new(SERVER)
        .args(["--http", "0"])
        .args(flags)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut line = String::new();
    within(BufReader::new(stdout).read_line(&mut line))
        .await
        .unwrap();
    (line.trim().to_owned(), child)
}

/// 一條連線,加上(HTTP 時)server 的子行程:server 要活得跟 client 一樣久。
pub struct Conn {
    pub client: McpClient,
    _server: Option<Child>,
}

impl Conn {
    pub async fn close(self) {
        self.client.close().await;
    }
}

/// `Deref`:`conn.list_tools()` 直接呼叫到 `conn.client.list_tools()`(像 Java 的委派,只是 compiler 自動做)。
impl Deref for Conn {
    type Target = McpClient;
    fn deref(&self) -> &McpClient {
        &self.client
    }
}

pub async fn transport(kind: Kind, flags: &[&str]) -> (Box<dyn Transport>, Option<Child>) {
    match kind {
        Kind::Stdio => {
            let mut args = vec!["--stdio".to_owned()];
            args.extend(flags.iter().map(|f| (*f).to_owned()));
            let config = StdioConfig {
                command: SERVER.into(),
                args,
                env: BTreeMap::new(),
            };
            (
                Box::new(StdioTransport::spawn(&config, StdioLimits::default()).unwrap()),
                None,
            )
        }
        Kind::Http => {
            let (url, child) = http_server(flags).await;
            let config = HttpConfig { url, token: None };
            (
                Box::new(HttpTransport::new(&config, HttpLimits::default()).unwrap()),
                Some(child),
            )
        }
    }
}

pub async fn connect_with(
    kind: Kind,
    flags: &[&str],
    options: ClientOptions,
) -> Result<Conn, McpError> {
    let (transport, server) = transport(kind, flags).await;
    let client = within(McpClient::connect(transport, options)).await?;
    Ok(Conn {
        client,
        _server: server,
    })
}

pub async fn connect(kind: Kind) -> Conn {
    connect_with(kind, &[], ClientOptions::default())
        .await
        .unwrap()
}
