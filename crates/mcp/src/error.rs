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

    /// server 違反協定(送來的不是合法的 JSON-RPC 訊息)。stdio:關掉連線,之後的訊息已經無法信任(fail-closed);
    /// HTTP:每個請求是獨立的 POST,只有這個請求失敗。
    #[error("the MCP server broke the protocol: {0}")]
    Protocol(String),

    /// server 送來的一則訊息超過上限。stdio 同樣關掉連線。
    #[error("a message from the MCP server is larger than {limit} bytes")]
    TooLarge { limit: usize },

    /// 連線已經結束(server 結束了、關掉了 stdout、寫入失敗)。`stderr` 是 server 最後印出的幾 KB,常常就是原因。
    #[error("the MCP server closed the connection{}", stderr_suffix(.stderr))]
    Closed { stderr: String },

    /// 等不到回應(`ClientOptions::request_timeout`)。這個請求已經送出取消通知,連線照常。
    #[error("the MCP server did not answer `{method}` in time")]
    Timeout { method: String },

    /// server 不支援我們的協定版本(只支援一個,design-part2 §9-2)。`supported` 是它說它支援的(已經整理成可以印的字串)。
    #[error(
        "the MCP server does not support protocol version {}; it supports: {}",
        crate::client::PROTOCOL_VERSION,
        list_or_none(.supported)
    )]
    UnsupportedVersion { supported: Vec<String> },

    /// `server/discover` 回了不是 2026-07-28 定義的錯誤,或是一直不回:多半是舊版(2025-11-25 以前、要 `initialize`)的 server。
    #[error(
        "the MCP server does not speak protocol version {version} (it may be an older server that needs `initialize`, \
         which this client does not support): {0}",
        version = crate::client::PROTOCOL_VERSION
    )]
    NotModern(String),

    /// server 的 capabilities 裡沒有 `tools`。
    #[error("the MCP server does not offer tools")]
    NoTools,

    /// 結果的格式不對(少了必填的欄位、型別錯、翻頁沒完沒了)。連線照常,只是這個結果不能用。
    #[error("the MCP server sent an invalid `{method}` result: {reason}")]
    InvalidResult { method: String, reason: String },

    /// HTTP 傳輸的 URL 不能用(不是 https、也不是本機的 http)。
    #[error("invalid MCP server URL: {0}")]
    InvalidUrl(String),

    /// HTTP 連不上、送不出去、讀到一半斷線。訊息不含 URL。
    #[error("could not reach the MCP server: {0}")]
    Network(String),

    /// HTTP 回了錯誤狀態碼,而且本文不是 JSON-RPC 的錯誤回應(那種會變成 `Rpc`)。`body` 是開頭一段、整理成可以印的。
    /// 3xx 也在這裡:我們不跟隨 redirect(token 不能被帶到別的主機)。
    #[error("the MCP server answered HTTP {status}{}", body_suffix(.body))]
    Http { status: u16, body: String },

    /// server 要求更多輸入(multi round-trip request)。我們沒有宣告任何 client 能力,不支援。
    #[error("the MCP server asked for more input, which this client does not support")]
    InputRequired,
}

fn body_suffix(body: &str) -> String {
    if body.is_empty() {
        String::new()
    } else {
        format!(": {body}")
    }
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".into()
    } else {
        items.join(", ")
    }
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

    #[test]
    fn http_shows_the_body_only_when_there_is_one() {
        assert_eq!(
            McpError::Http {
                status: 502,
                body: String::new()
            }
            .to_string(),
            "the MCP server answered HTTP 502"
        );
        assert_eq!(
            McpError::Http {
                status: 401,
                body: "unauthorized".into()
            }
            .to_string(),
            "the MCP server answered HTTP 401: unauthorized"
        );
    }

    #[test]
    fn unsupported_version_lists_what_the_server_supports() {
        assert_eq!(
            McpError::UnsupportedVersion {
                supported: vec!["2025-06-18".into(), "2025-11-25".into()]
            }
            .to_string(),
            "the MCP server does not support protocol version 2026-07-28; it supports: 2025-06-18, 2025-11-25"
        );
        assert_eq!(
            McpError::UnsupportedVersion { supported: vec![] }.to_string(),
            "the MCP server does not support protocol version 2026-07-28; it supports: (none)"
        );
    }
}
