//! 傳輸:把 JSON-RPC 訊息送到 server、把回應拿回來(design-part2 §5,D-048)。
//!
//! 兩種實作:stdio(子行程,這一步)與 Streamable HTTP(第 4 步)。`McpClient`(第 3 步)只看得到這個 trait,
//! 不知道底下是哪一種。

use futures::future::BoxFuture;
use serde_json::Value;

use crate::McpError;

pub mod stdio;

pub use stdio::{StdioConfig, StdioLimits, StdioTransport};

/// 一條到 MCP server 的連線。
///
/// 為什麼回傳 `BoxFuture` 而不是寫 `async fn`:trait 裡的 `async fn` 不能用在 `dyn Transport` 上
/// (每個實作的 future 型別不同、大小不同)。`BoxFuture` 把 future 放到 heap 上、抹除成同一個型別,
/// 跟第一部分的 `ErasedTool` 一樣。Java 的介面方法回傳 `CompletableFuture<Value>` 就是這個意思。
pub trait Transport: Send + Sync {
    /// 送出一個請求,等它的回應。id 由傳輸自己配(同一條連線上不重複)。
    /// server 回 `error` 時是 `Err(McpError::Rpc(..))`。
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<Value, McpError>>;

    /// 送出一個通知(不等回應)。
    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>>;

    /// 結束連線。`self: Box<Self>`:拿走所有權,關掉之後這條連線就不能再用(compiler 保證,不是執行期檢查)。
    /// 回傳的 future 是 `'static`:它不再借用任何東西,可以丟給別的 task 去等。
    fn close(self: Box<Self>) -> BoxFuture<'static, ()>;
}
