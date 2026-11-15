//! MCP client 的錯誤。

use crate::jsonrpc::RpcError;

/// 跟 MCP server 溝通時的錯誤。
///
/// `Clone`:連線斷掉時,原因要交給**每一個**還在等回應的請求(一個 `McpError` 複製多份)。
/// 所以欄位只放字串,不放 `std::io::Error`(它不能 clone)。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum McpError {
    /// 子行程啟動不了(找不到指令、沒有執行權限)。
    #[error("could not start the MCP server `{command}`: {reason}")]
    Spawn { command: String, reason: String },

    /// server 回了 JSON-RPC 的錯誤回應。連線還在,之後的請求照常。
    #[error("the MCP server returned an error: {0}")]
    Rpc(RpcError),

    /// server 違反協定(送來的不是合法的 JSON-RPC 訊息)。我們關掉連線:之後的訊息已經無法信任(fail-closed)。
    #[error("the MCP server broke the protocol, connection closed: {0}")]
    Protocol(String),

    /// server 送來的一則訊息超過上限。同樣關掉連線。
    #[error("a message from the MCP server is larger than {limit} bytes, connection closed")]
    TooLarge { limit: usize },

    /// 連線已經結束(server 結束了、關掉了 stdout、寫入失敗)。`stderr` 是 server 最後印出的幾 KB,常常就是原因。
    #[error("the MCP server closed the connection{}", stderr_suffix(.stderr))]
    Closed { stderr: String },
}

/// `#[error]` 裡可以呼叫函式:`.stderr` 是欄位的參考。stderr 是空的就不印後半段。
fn stderr_suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!("; its stderr ends with:\n{stderr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_shows_the_stderr_tail_only_when_there_is_one() {
        assert_eq!(
            McpError::Closed {
                stderr: String::new()
            }
            .to_string(),
            "the MCP server closed the connection"
        );
        assert_eq!(
            McpError::Closed {
                stderr: "boom".into()
            }
            .to_string(),
            "the MCP server closed the connection; its stderr ends with:\nboom"
        );
    }
}
