//! 沙箱:在隔離的容器裡編譯、執行不可信的單檔 Rust(003 第三部分,設計見 `docs/modules/003-agent-runtime/design-part3.md`)。
//!
//! 「編譯也是執行」:`include_str!("/home/me/.ssh/id_ed25519")` 在**編譯時**讀檔,所以編譯也要在沙箱裡(design-part3 §1)。
//! 這個 crate 不知道 agent、eval 存在;它們只看得到 [`Sandbox`] trait。

use std::time::Duration;

use futures::future::BoxFuture;

mod container;
mod error;

pub use container::{ContainerConfig, ContainerSandbox, DEFAULT_IMAGE, DEFAULT_PROGRAM};
pub use error::SandboxError;

/// 要做到哪一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 只編譯(eval 的 `compiles`,P3-6)。
    Compile,
    /// 編譯,成功的話再執行(agent 的 `run_rust`)。
    Run,
}

/// 沙箱的上限(design-part3 §2 的「小決定」)。每一項都必須大於 0:可以調整、不能關掉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// 編譯的時間上限。
    pub compile_timeout: Duration,
    /// 執行的時間上限。
    pub run_timeout: Duration,
    /// 整個容器的記憶體(編譯器與程式共用;不能用 swap 繞過)。
    pub memory_bytes: u64,
    /// 整個容器同時存在的行程數(執行緒也算)。擋 fork bomb。
    pub pids: u32,
    /// stdout、stderr **各自**最多保留幾個位元組;超過的部分讀掉丟棄,`Output::truncated` 設成 true。
    pub output_bytes: usize,
    /// 工作目錄(tmpfs,放在記憶體裡)的大小。原始碼、編譯產物、程式寫的檔案都在這裡。
    pub scratch_bytes: u64,
    /// 原始碼的大小上限。超過的話根本不啟動容器。
    pub code_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            compile_timeout: Duration::from_secs(20),
            run_timeout: Duration::from_secs(5),
            memory_bytes: 512 * 1024 * 1024,
            pids: 64,
            output_bytes: 64 * 1024,
            scratch_bytes: 64 * 1024 * 1024,
            code_bytes: 64 * 1024,
        }
    }
}

/// 一個階段(編譯或執行)的結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// 結束碼。逾時被收掉時是 `None`。被 OOM killer 或訊號結束時,docker 回報 128 + 訊號編號(例如 137 = SIGKILL)。
    pub exit_code: Option<i32>,
    /// stdout 的開頭(最多 `Limits::output_bytes`)。不是合法 UTF-8 的位元組換成 U+FFFD。
    pub stdout: String,
    /// stderr 的開頭。編譯失敗時,錯誤訊息在這裡。
    pub stderr: String,
    /// 超過時間上限,被收掉了。
    pub timed_out: bool,
    /// stdout 或 stderr 超過上限,後面被丟掉了。
    pub truncated: bool,
}

impl Output {
    /// 結束碼是 0、而且沒有逾時。
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// 一次執行的報告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub compile: Output,
    /// 執行的結果。`Action::Compile`、或編譯失敗時是 `None`。
    pub run: Option<Output>,
}

/// 能編譯、執行單檔 Rust 的隔離環境。
///
/// 回傳 `BoxFuture` 而不是寫 `async fn`:原因同 mcp 的 `Transport`(要能用在 `dyn Sandbox` 上)。
/// Java 對照:`interface Sandbox { CompletableFuture<Report> rust(String source, Action action); }`。
pub trait Sandbox: Send + Sync {
    /// 編譯(與執行)`source`。程式的問題(編譯錯誤、結束碼不是 0、逾時)都在 `Report` 裡;
    /// `Err` 只代表沙箱本身不能用(找不到 docker、image 不在、程式碼太大)。
    fn rust<'a>(
        &'a self,
        source: &'a str,
        action: Action,
    ) -> BoxFuture<'a, Result<Report, SandboxError>>;
}
