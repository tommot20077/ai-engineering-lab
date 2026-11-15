//! stdio 傳輸:server 是子行程,一行一則 JSON-RPC 訊息(design-part2 §5)。
//!
//! ```text
//!             stdin(我們寫)                     ┌──────────────┐
//!   request ──────────────────────────────────→  │              │
//!                                                │  MCP server  │
//!   讀 stdout 的背景 task ←─────────────────────  │  (子行程)    │
//!     依 id 交給等待中的請求(oneshot)            │              │
//!   讀 stderr 的背景 task ←─────────────────────  └──────────────┘
//!     只留最後 4 KiB,錯誤訊息用
//! ```
//!
//! 請求與回應怎麼對上:每個請求配一個 id,在等待名單(`HashMap<id, oneshot::Sender>`)登記之後才送出;
//! 讀 stdout 的 task 收到回應,用 id 找到登記的 `Sender`,把結果送過去。回應的順序可以跟請求不同。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::Transport;
use crate::McpError;
use crate::jsonrpc::{self, Id, Incoming, METHOD_NOT_FOUND};

/// 錯誤訊息裡附上的 stderr:最後這麼多位元組。
pub const STDERR_TAIL_BYTES: usize = 4096;

/// stdout 結束之後,等 stderr 讀完的上限。server 結束時兩個 pipe 幾乎同時關閉,
/// 但讀 stderr 的 task 可能還沒讀到最後一段;等一下,錯誤訊息才會有 server 最後說的話。
const STDERR_SETTLE: Duration = Duration::from_secs(1);

/// 怎麼啟動 server。
///
/// `env` 就是子行程的**全部**環境變數(D-050):其他的一律不繼承,API key 不會漏給第三方程式。
/// 要讓 server 找得到其他程式(`PATH`)、知道家目錄(`HOME`),由組裝的地方(agent 的 CLI)明確放進來:
/// library 不讀環境變數(D-016)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StdioConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// 上限。數值放 `Default`,測試可以改小(同 D-016 的 `Limits`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdioLimits {
    /// server 送來的一則訊息(一行,不含換行)的上限。超過就關掉連線。
    pub max_message_bytes: usize,
    /// 關閉時,關掉 stdin 之後等 server 自己結束的時間;超過就 kill。
    pub close_grace: Duration,
}

impl Default for StdioLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: 4 * 1024 * 1024,
            close_grace: Duration::from_secs(2),
        }
    }
}

/// 等待名單裡的一筆:回應來了,從這裡送給等待中的請求。
type Pending = oneshot::Sender<Result<Value, McpError>>;

/// 連線的狀態。請求端與讀 stdout 的 task 共用,所以包在 `Arc<Mutex<..>>` 裡。
enum State {
    Open(HashMap<i64, Pending>),
    /// 連線結束了,原因留著:之後的請求立刻拿到同一個原因。
    Closed(McpError),
}

struct Shared {
    // std 的 `Mutex` 就夠了:鎖只拿來改 HashMap,不會跨 `.await` 拿著(拿著鎖 `.await` 要用 tokio 的 `Mutex`)。
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // 別的執行緒拿著鎖 panic 時,鎖會被標成 poisoned;資料本身沒問題(HashMap 的操作不會做一半),照常用。
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 在等待名單登記 id。連線已經結束就直接回傳原因。
    fn register(&self, id: i64) -> Result<oneshot::Receiver<Result<Value, McpError>>, McpError> {
        match &mut *self.lock() {
            State::Open(pending) => {
                let (tx, rx) = oneshot::channel();
                pending.insert(id, tx);
                Ok(rx)
            }
            State::Closed(reason) => Err(reason.clone()),
        }
    }

    /// 回應來了:交給登記過的請求。不認得的 id 略過(可能是已經放棄等待的請求,晚到的回應)。
    fn deliver(&self, id: Id, outcome: Result<Value, jsonrpc::RpcError>) {
        // 我們送出的 id 都是數字;字串 id 一定不是回我們的。
        let Id::Number(id) = id else { return };
        let waiting = match &mut *self.lock() {
            State::Open(pending) => pending.remove(&id),
            State::Closed(_) => None,
        };
        if let Some(tx) = waiting {
            // 對方已經不等了(future 被 drop)也沒關係。
            let _ = tx.send(outcome.map_err(McpError::Rpc));
        }
    }

    fn forget(&self, id: i64) {
        if let State::Open(pending) = &mut *self.lock() {
            pending.remove(&id);
        }
    }

    /// 連線結束:記下原因,通知所有還在等的請求。
    fn close(&self, reason: McpError) {
        // `mem::replace`:把舊的狀態拿出來、同時放進新的。Java 沒有「搬走」的概念,這裡拿到的是 HashMap 本身,不是複本。
        let old = std::mem::replace(&mut *self.lock(), State::Closed(reason.clone()));
        if let State::Open(pending) = old {
            for tx in pending.into_values() {
                let _ = tx.send(Err(reason.clone()));
            }
        }
    }

    fn reason(&self) -> Option<McpError> {
        match &*self.lock() {
            State::Open(_) => None,
            State::Closed(reason) => Some(reason.clone()),
        }
    }

    fn in_flight(&self) -> usize {
        match &*self.lock() {
            State::Open(pending) => pending.len(),
            State::Closed(_) => 0,
        }
    }
}

/// 請求的 future 被 drop 時(逾時、呼叫端被取消),把它從等待名單拿掉。
///
/// RAII:Rust 沒有 `finally`,「離開時一定要做的事」放在一個值的 `Drop` 裡;不管正常回傳、`?` 提早回傳、
/// 還是 future 被丟掉,這個值都會被 drop。Java 的 try-with-resources 是同一個想法,只是 Rust 不必寫 `try`。
struct Forget<'a> {
    shared: &'a Shared,
    id: i64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.shared.forget(self.id);
    }
}

/// stdin 由請求端與讀 stdout 的 task(回應 server 的反問)共用;關閉時要能拿走(`Option::take`),讓 server 看到 EOF。
/// 這裡用 tokio 的 `Mutex`:寫入要 `.await`,鎖要跨 `.await` 拿著,一則訊息才不會跟別的訊息交錯。
type SharedStdin = Arc<tokio::sync::Mutex<Option<ChildStdin>>>;

/// stdio 傳輸。
pub struct StdioTransport {
    child: Child,
    stdin: SharedStdin,
    shared: Arc<Shared>,
    tail: Arc<Mutex<Tail>>,
    reader: JoinHandle<()>,
    next_id: AtomicI64,
    close_grace: Duration,
}

impl StdioTransport {
    /// 啟動 server。要在 tokio runtime 裡呼叫(背景 task 需要它)。
    pub fn spawn(config: &StdioConfig, limits: StdioLimits) -> Result<Self, McpError> {
        let spawn_error = |reason: String| McpError::Spawn {
            command: config.command.clone(),
            reason,
        };
        let mut child = Command::new(&config.command)
            .args(&config.args)
            // 清空環境變數,只放設定檔寫的(D-050)。Java 的 `ProcessBuilder.environment().clear()`。
            .env_clear()
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // `Child` 被 drop 時 kill 子行程。agent 不管怎麼結束(包括 panic、`?` 提早回傳),server 都不會留下來。
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| spawn_error(e.to_string()))?;

        // `take()`:把 pipe 從 `Child` 裡拿出來,所有權交給各自的使用者。三個都有設 `piped()`,一定拿得到。
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(spawn_error("the stdio pipes were not created".into()));
        };

        let stdin: SharedStdin = Arc::new(tokio::sync::Mutex::new(Some(stdin)));
        let shared = Arc::new(Shared {
            state: Mutex::new(State::Open(HashMap::new())),
        });
        let tail = Arc::new(Mutex::new(Tail::new(STDERR_TAIL_BYTES)));
        let stderr_task = tokio::spawn(drain_stderr(stderr, Arc::clone(&tail)));
        let reader = tokio::spawn(read_stdout(ReadStdout {
            stdout,
            stdin: Arc::clone(&stdin),
            shared: Arc::clone(&shared),
            tail: Arc::clone(&tail),
            stderr_task,
            limit: limits.max_message_bytes,
        }));

        Ok(Self {
            child,
            stdin,
            shared,
            tail,
            reader,
            next_id: AtomicI64::new(1),
            close_grace: limits.close_grace,
        })
    }

    /// 還在等回應的請求數。
    pub fn in_flight(&self) -> usize {
        self.shared.in_flight()
    }

    /// 結束連線:關掉 stdin(server 看到 EOF 應該自己結束)→ 最多等 `close_grace` → 還沒結束就 kill。
    /// 回傳 server 的結束狀態(拿不到時是 `None`)。
    pub async fn shutdown(self) -> Option<ExitStatus> {
        // 解構:把欄位一個一個拿出來。`StdioTransport` 沒有實作 `Drop`,才能這樣拆開。
        let Self {
            mut child,
            stdin,
            reader,
            close_grace,
            ..
        } = self;
        // 拿走 stdin 再 drop,pipe 就關了。讀 stdout 的 task 也拿著同一個 `Arc`,所以要 `take()` 而不是 drop `Arc`。
        drop(stdin.lock().await.take());
        let status = match tokio::time::timeout(close_grace, child.wait()).await {
            Ok(status) => status.ok(),
            Err(_elapsed) => {
                // tokio 的 `kill()` 送出 SIGKILL 之後會等行程結束,不會留下 zombie。
                let _ = child.kill().await;
                child.wait().await.ok()
            }
        };
        // server 的子行程可能還拿著 stdout,讀的 task 就不會結束;不再需要它了。
        reader.abort();
        status
    }

    /// 送出一行。寫不出去代表連線已經斷了:回傳讀 stdout 的 task 記下的原因(通常更具體),還沒有的話就是 `Closed`。
    async fn send(&self, line: &str) -> Result<(), McpError> {
        write_line(&self.stdin, line).await.map_err(|_| {
            self.shared.reason().unwrap_or_else(|| McpError::Closed {
                stderr: lock(&self.tail).text(),
            })
        })
    }

    async fn call(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        // `fetch_add`:多個請求同時進來也不會拿到同一個 id。`Relaxed`:只需要數字不重複,不必跟其他記憶體操作排序。
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // 先登記再送出:回應可能在 `send` 回傳之前就到了。
        let response = self.shared.register(id)?;
        let _forget = Forget {
            shared: &self.shared,
            id,
        };
        self.send(&jsonrpc::request(id, method, params)).await?;
        // `Sender` 沒送東西就被丟掉:不會發生(`Shared::close` 會把原因送給每一個),型別上還是要處理。
        response.await.unwrap_or(Err(McpError::Closed {
            stderr: String::new(),
        }))
    }
}

impl Transport for StdioTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(self.call(method, params))
    }

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            if let Some(reason) = self.shared.reason() {
                return Err(reason);
            }
            self.send(&jsonrpc::notification(method, params)).await
        })
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        // `*self`:把 `Box` 裡的值搬出來,交給拿所有權的 `shutdown`。
        Box::pin(async move {
            let _ = (*self).shutdown().await;
        })
    }
}

fn lock(tail: &Mutex<Tail>) -> MutexGuard<'_, Tail> {
    tail.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 一則訊息一次寫完:訊息本身加換行,拿著鎖寫完再 flush。
async fn write_line(
    stdin: &tokio::sync::Mutex<Option<ChildStdin>>,
    line: &str,
) -> std::io::Result<()> {
    let mut guard = stdin.lock().await;
    let pipe = guard
        .as_mut()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
    pipe.write_all(format!("{line}\n").as_bytes()).await?;
    pipe.flush().await
}

/// 讀 stdout 的 task 需要的東西。參數太多,包成一個 struct。
struct ReadStdout {
    stdout: ChildStdout,
    stdin: SharedStdin,
    shared: Arc<Shared>,
    tail: Arc<Mutex<Tail>>,
    stderr_task: JoinHandle<()>,
    limit: usize,
}

/// 背景 task:一行一行讀 stdout,直到連線結束;結束的原因交給 `Shared::close`。
async fn read_stdout(task: ReadStdout) {
    let ReadStdout {
        stdout,
        stdin,
        shared,
        tail,
        mut stderr_task,
        limit,
    } = task;
    let mut reader = BufReader::new(stdout);
    // `loop` 也是表達式:`break 值` 就是整個 loop 的值。
    let reason = loop {
        let bytes = match read_message(&mut reader, limit).await {
            Ok(Some(bytes)) => bytes,
            Err(LineError::TooLarge) => break McpError::TooLarge { limit },
            // 正常結束、斷在一半、讀取錯誤:都是 server 走了。等 stderr 讀完,錯誤訊息才有 server 最後說的話。
            Ok(None) | Err(LineError::Truncated | LineError::Io(_)) => {
                // `&mut stderr_task`:只是等它,不拿走所有權(`JoinHandle` 本身就是 future)。
                let _ = tokio::time::timeout(STDERR_SETTLE, &mut stderr_task).await;
                break McpError::Closed {
                    stderr: lock(&tail).text(),
                };
            }
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            break McpError::Protocol("not UTF-8".into());
        };
        // 空行略過(有些 server 會多印一個換行);JSON 本身允許前後有空白,`\r\n` 的 `\r` 也是空白。
        if text.trim().is_empty() {
            continue;
        }
        match jsonrpc::parse(text) {
            Ok(Incoming::Response { id, outcome }) => shared.deliver(id, outcome),
            // server 反問(sampling 之類):不支援(design-part2 §1)。不把對方的 method 放進訊息:不回顯不可信的內容。
            Ok(Incoming::Request { id, .. }) => {
                let reply = jsonrpc::error_response(
                    &id,
                    METHOD_NOT_FOUND,
                    "this client only supports tools",
                );
                // 寫不出去的話,下一次讀 stdout 就會看到 EOF,在那裡收尾。
                let _ = write_line(&stdin, &reply).await;
            }
            // 通知(`notifications/tools/list_changed` 之類):白名單固定,不跟著變,略過(§4)。
            Ok(Incoming::Notification { .. }) => {}
            Err(e) => break McpError::Protocol(e.to_string()),
        }
    };
    shared.close(reason);
}

/// 背景 task:一直讀 stderr,只留最後一段。不讀的話 pipe 滿了,server 寫 stderr 時會卡住(連帶 stdout 也不動了)。
async fn drain_stderr(mut stderr: ChildStderr, tail: Arc<Mutex<Tail>>) {
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => lock(&tail).push(chunk.get(..n).unwrap_or_default()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LineError {
    /// 還沒讀到換行就超過上限。
    TooLarge,
    /// 讀到一半就結束了(最後一行沒有換行)。
    Truncated,
    Io(String),
}

/// 讀一則訊息(一行,不含換行)。`Ok(None)` 是正常結束。
///
/// 泛型 `R: AsyncBufRead`:正式用的是子行程的 stdout,測試直接給 `&[u8]`。
/// `take(limit + 1)` 讓一次最多讀 `limit + 1` 個位元組:對方一直不送換行,我們也不會一直讀下去、記憶體一直長。
async fn read_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<Vec<u8>>, LineError> {
    let mut line = Vec::new();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    // `&mut *reader`:借一個可變參考給 `take`(它要拿走所有權),讀完 reader 還是我們的。
    let n = (&mut *reader)
        .take(cap)
        .read_until(b'\n', &mut line)
        .await
        .map_err(|e| LineError::Io(e.to_string()))?;
    if n == 0 {
        return Ok(None);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        return Ok(Some(line));
    }
    // 沒有換行:不是讀到上限,就是輸入結束了。
    if line.len() > limit {
        Err(LineError::TooLarge)
    } else {
        Err(LineError::Truncated)
    }
}

/// stderr 的最後一段。`VecDeque`:從後面加、從前面丟,兩邊都是 O(1)(Java 的 `ArrayDeque`)。
struct Tail {
    bytes: VecDeque<u8>,
    cap: usize,
}

impl Tail {
    fn new(cap: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(cap),
            cap,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        let excess = self.bytes.len().saturating_sub(self.cap);
        self.bytes.drain(..excess);
    }

    /// 給人看的文字:不合法的 UTF-8 換成 U+FFFD,控制字元換成 `?`(stderr 會被印到終端機)。
    fn text(&self) -> String {
        // `VecDeque` 在記憶體裡可能繞一圈,分成兩段;接起來才是連續的位元組。
        let (front, back) = self.bytes.as_slices();
        let bytes = [front, back].concat();
        String::from_utf8_lossy(&bytes)
            .chars()
            .filter(|&c| c != '\r')
            .map(|c| {
                if c.is_control() && c != '\n' && c != '\t' {
                    '?'
                } else {
                    c
                }
            })
            .collect::<String>()
            .trim()
            .to_owned()
    }
}

#[cfg(test)]
mod tests;
