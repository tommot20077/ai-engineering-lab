//! `ContainerSandbox`:用 `docker`(或相容的 `podman`)在容器裡編譯、執行(design-part3 §2 P3-2、P3-7)。
//!
//! 一次執行一個新容器,分四步,每一步都有主機這邊的逾時:
//!
//! ```text
//! docker run --detach … image sleep N    啟動一個什麼都不做的容器(隔離參數都在這裡)
//! docker exec --interactive … cat        原始碼從 stdin 寫進 /work/main.rs
//! docker exec … rustc …                  編譯(compile_timeout)
//! docker exec … /work/main               執行(run_timeout)
//! docker rm --force                      不管成功、失敗、逾時、被取消,一定收掉
//! ```
//!
//! 為什麼不是一個 `docker run … sh -c "rustc && ./main"`:編譯與執行要分開計時、分開收輸出;
//! 而且逾時要由**主機**判斷 —— 容器裡的 `timeout` 跟不可信的程式是同一個使用者,程式可以把它 kill 掉。

use std::process::Stdio;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::{Action, Limits, Output, Report, Sandbox, SandboxError};

/// 預設的容器指令。
pub const DEFAULT_PROGRAM: &str = "docker";

/// 預設的 image:官方 `rust:<版本>-slim`,版本跟 `rust-toolchain.toml` 一致,用 digest 鎖定(P3-7)。
/// 寫完整名稱(`docker.io/library/…`):podman 不一定會把短名稱解析到 Docker Hub。
pub const DEFAULT_IMAGE: &str = "docker.io/library/rust:1.99.1-slim@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7";

/// 容器名稱的前綴,後面接一個 UUID。
const NAME_PREFIX: &str = "ai-lab-sandbox-";
/// 每個沙箱容器都貼這個 label:出事時可以用 `docker ps --filter label=…` 找出來(逃逸測試也用它確認沒有留下容器)。
const LABEL: &str = "ai-engineering-lab.sandbox=1";
/// nobody。image 裡不需要有這個使用者,uid 就夠。
const USER: &str = "65534:65534";

/// `docker run --detach` 的上限:只是建立容器,但 Mac 上的 Docker Desktop 第一次可能要幾秒。
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// 寫入原始碼的上限(最多 `code_bytes`,通常是瞬間)。
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// `docker rm --force` 的上限。
const REMOVE_TIMEOUT: Duration = Duration::from_secs(30);
/// 容器裡的 `sleep` 比所有步驟加起來再多這麼久。
const SLEEP_SLACK: Duration = Duration::from_secs(30);
/// 容器指令自己的錯誤訊息(stderr)最多留多少放進 `SandboxError`。
const DETAIL_BYTES: usize = 1024;

/// `ContainerSandbox` 的設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerConfig {
    /// `docker` 或 `podman`(兩者的 CLI 參數相容),也可以是完整路徑。
    pub program: String,
    /// 必須用 digest 鎖定(`name@sha256:…`):tag 可以被換成別的內容,digest 不行。
    pub image: String,
    pub limits: Limits,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            program: DEFAULT_PROGRAM.to_owned(),
            image: DEFAULT_IMAGE.to_owned(),
            limits: Limits::default(),
        }
    }
}

/// 用容器隔離的沙箱。
#[derive(Debug)]
pub struct ContainerSandbox {
    config: ContainerConfig,
}

impl ContainerSandbox {
    /// 檢查設定;不合法就回 `InvalidConfig`(建立時就失敗,不要等到第一次執行)。
    /// 這裡不檢查 docker 在不在、image 有沒有下載:那些要真的執行才知道,而且可能之後才裝好。
    pub fn new(config: ContainerConfig) -> Result<Self, SandboxError> {
        if config.program.is_empty() {
            return Err(SandboxError::InvalidConfig(
                "the container program must not be empty".to_owned(),
            ));
        }
        if !is_pinned(&config.image) {
            return Err(SandboxError::InvalidConfig(format!(
                "the image `{}` must be pinned by digest (`name@sha256:<64 hex digits>`)",
                config.image
            )));
        }
        check_limits(&config.limits).map_err(SandboxError::InvalidConfig)?;
        Ok(Self { config })
    }

    pub fn config(&self) -> &ContainerConfig {
        &self.config
    }

    async fn execute(&self, source: &str, action: Action) -> Result<Report, SandboxError> {
        let limits = &self.config.limits;
        if source.len() > limits.code_bytes {
            return Err(SandboxError::TooLarge {
                limit: limits.code_bytes,
            });
        }
        let program = self.config.program.as_str();
        let name = format!("{NAME_PREFIX}{}", uuid::Uuid::now_v7().simple());

        // 先建立守衛、再啟動:就算 `docker run` 逾時(容器可能其實起來了),drop 時也會送 rm。
        let container = Container::new(program, name);
        let result = self.steps(&container, source, action).await;
        container.remove().await;
        result
    }

    /// 啟動、寫入、編譯、執行。收掉容器由呼叫端負責(不管這裡回傳什麼)。
    async fn steps(
        &self,
        container: &Container,
        source: &str,
        action: Action,
    ) -> Result<Report, SandboxError> {
        let limits = &self.config.limits;
        let program = container.program.as_str();
        let name = container.name.as_str();

        let start = command(
            program,
            &start_args(&self.config.image, limits, name),
            None,
            START_TIMEOUT,
            DETAIL_BYTES,
        )
        .await?;
        if !start.success() {
            return Err(self.start_error(&start));
        }

        let write = command(
            program,
            &write_args(name),
            Some(source.as_bytes()),
            WRITE_TIMEOUT,
            DETAIL_BYTES,
        )
        .await?;
        if !write.success() {
            return Err(failure("could not write the source file", &write));
        }

        let compile = command(
            program,
            &compile_args(name),
            None,
            limits.compile_timeout,
            limits.output_bytes,
        )
        .await?;
        if action == Action::Compile || !compile.success() {
            return Ok(Report { compile, run: None });
        }

        let run = command(
            program,
            &exec_args(name),
            None,
            limits.run_timeout,
            limits.output_bytes,
        )
        .await?;
        Ok(Report {
            compile,
            run: Some(run),
        })
    }

    fn start_error(&self, start: &Output) -> SandboxError {
        // docker:「Unable to find image」(搭配 --pull never)/「No such image」;podman:「image not known」。
        let missing = ["Unable to find image", "No such image", "image not known"]
            .iter()
            .any(|needle| start.stderr.contains(needle));
        if missing {
            SandboxError::ImageMissing {
                program: self.config.program.clone(),
                image: self.config.image.clone(),
            }
        } else {
            failure("could not start the container", start)
        }
    }
}

impl Sandbox for ContainerSandbox {
    fn rust<'a>(
        &'a self,
        source: &'a str,
        action: Action,
    ) -> BoxFuture<'a, Result<Report, SandboxError>> {
        Box::pin(self.execute(source, action))
    }
}

/// 沙箱自己的步驟失敗(不是程式的問題)。
fn failure(what: &str, output: &Output) -> SandboxError {
    let reason = if output.timed_out {
        "timed out".to_owned()
    } else {
        printable(output.stderr.as_bytes())
    };
    SandboxError::Container {
        detail: format!("{what}: {reason}"),
    }
}

/// `name@sha256:<64 個小寫 hex>`,而且整個字串沒有別的東西(空白之類的)。
fn is_pinned(image: &str) -> bool {
    let Some((name, digest)) = image.split_once('@') else {
        return false;
    };
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    !name.is_empty()
        && !name.contains(char::is_whitespace)
        && hex.len() == 64
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn check_limits(limits: &Limits) -> Result<(), String> {
    let zero = [
        ("compile_timeout", limits.compile_timeout.is_zero()),
        ("run_timeout", limits.run_timeout.is_zero()),
        ("memory_bytes", limits.memory_bytes == 0),
        ("pids", limits.pids == 0),
        ("output_bytes", limits.output_bytes == 0),
        ("scratch_bytes", limits.scratch_bytes == 0),
        ("code_bytes", limits.code_bytes == 0),
    ];
    match zero.iter().find(|(_, is_zero)| *is_zero) {
        Some((name, _)) => Err(format!("the sandbox limit `{name}` must be greater than 0")),
        None => Ok(()),
    }
}

/// `docker run` 的參數:隔離全部在這裡(design-part3 §2 P3-2)。單元測試一個一個鎖住。
pub(crate) fn start_args(image: &str, limits: &Limits, name: &str) -> Vec<String> {
    let memory = limits.memory_bytes.to_string();
    let args = [
        "run",
        "--detach",
        // 容器結束就刪掉(PID 1 的 sleep 到期、或被 rm --force)。
        "--rm",
        "--name",
        name,
        "--label",
        LABEL,
        // 不自動下載:image 不在本機就失敗(SandboxError::ImageMissing)。
        "--pull",
        "never",
        // 沒有網路介面(只有 lo):不能把資料送出去,也不能下載東西。
        "--network",
        "none",
        // 根目錄唯讀;唯一能寫的是下面這個 tmpfs(放在記憶體裡,容器結束就消失)。
        "--read-only",
        "--tmpfs",
        // exec:編譯出來的執行檔要能執行(docker 的 tmpfs 預設是 noexec);擁有者是 nobody,其他人不能進。
        &format!(
            "/work:rw,exec,nosuid,nodev,size={},uid=65534,gid=65534,mode=0700",
            limits.scratch_bytes
        ),
        "--workdir",
        "/work",
        // rustc 與連結器的暫存檔也放進 /work(根目錄唯讀,/tmp 寫不進去)。
        "--env",
        "TMPDIR=/work",
        // 不是 root;拿掉所有 capability;setuid 的程式也拿不到更多權限。
        "--user",
        USER,
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        // memory-swap 跟 memory 一樣大 = 不能用 swap 繞過記憶體上限。
        "--memory",
        &memory,
        "--memory-swap",
        &memory,
        "--pids-limit",
        &limits.pids.to_string(),
        "--cpus",
        "1",
        // PID 1 的輸出不需要留下來(它是 sleep,什麼都不印;exec 的輸出本來就不進 log)。
        "--log-driver",
        "none",
        image,
        "sleep",
        &sleep_secs(limits).to_string(),
    ];
    args.iter().map(|&a| a.to_owned()).collect()
}

/// 容器的 PID 1 睡多久:所有步驟的上限加起來再加寬限。我們的程式被 SIGKILL、來不及收掉容器時,容器時間到自己結束,
/// `--rm` 把它刪掉 —— 不會有容器永遠留著。
pub(crate) fn sleep_secs(limits: &Limits) -> u64 {
    let total = START_TIMEOUT + WRITE_TIMEOUT + SLEEP_SLACK;
    // 不足一秒的部分進位:sleep 只收整數秒,不能比上限短。
    total.as_secs() + ceil_secs(limits.compile_timeout) + ceil_secs(limits.run_timeout)
}

fn ceil_secs(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

/// 原始碼從 stdin 寫進容器:內容不經過命令列,不會被 shell 解讀(`sh -c` 的字串是固定的)。
pub(crate) fn write_args(name: &str) -> Vec<String> {
    to_strings(&[
        "exec",
        "--interactive",
        name,
        "sh",
        "-c",
        "cat > /work/main.rs",
    ])
}

pub(crate) fn compile_args(name: &str) -> Vec<String> {
    to_strings(&[
        "exec",
        name,
        "rustc",
        "--edition",
        "2024",
        "--crate-name",
        "main",
        "-o",
        "/work/main",
        "/work/main.rs",
    ])
}

pub(crate) fn exec_args(name: &str) -> Vec<String> {
    to_strings(&["exec", name, "/work/main"])
}

fn to_strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|&a| a.to_owned()).collect()
}

/// 執行容器指令一次:寫入 stdin(如果有)、同時讀 stdout 與 stderr(各自最多 `cap`)、等它結束,全部在 `timeout` 內。
///
/// 逾時:回傳到目前為止讀到的輸出、`timed_out: true`。`docker` 這個 client 行程被 `kill_on_drop` 收掉;
/// 容器裡的行程**不會**跟著結束,由呼叫端的 `docker rm --force` 收掉整個容器。
async fn command(
    program: &str,
    args: &[String],
    stdin: Option<&[u8]>,
    timeout: Duration,
    cap: usize,
) -> Result<Output, SandboxError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| SandboxError::Unavailable {
            program: program.to_owned(),
            reason: e.to_string(),
        })?;
    let input = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // 緩衝區放在 future 外面:逾時的時候 future 被 drop,借用結束,已經讀到的部分還在。
    // Java 對照:像把 StringBuilder 傳進另一個執行緒,逾時之後主執行緒照樣讀得到它 —— 但這裡由 compiler 確認
    // future 結束(被 drop)之後才能再碰它,不會同時有兩邊在用。
    let mut out = Vec::new();
    let mut err = Vec::new();
    let (out_cut, err_cut, status) = {
        let all = async {
            // stdin 先寫完再關掉(drop),`cat` 才會讀到 EOF。寫入失敗(對方提早結束)不用管:結束碼會說明。
            let feed = async {
                if let (Some(mut pipe), Some(bytes)) = (input, stdin) {
                    let _ = pipe.write_all(bytes).await;
                }
            };
            let (_, out_cut, err_cut, status) = tokio::join!(
                feed,
                read_optional(stdout, cap, &mut out),
                read_optional(stderr, cap, &mut err),
                child.wait(),
            );
            (out_cut, err_cut, status)
        };
        match tokio::time::timeout(timeout, all).await {
            Ok(done) => done,
            Err(_elapsed) => {
                return Ok(Output {
                    exit_code: None,
                    stdout: lossy(out),
                    stderr: lossy(err),
                    timed_out: true,
                    truncated: false,
                });
            }
        }
    };
    let exit_code = status
        .map_err(|e| SandboxError::Unavailable {
            program: program.to_owned(),
            reason: e.to_string(),
        })?
        .code();
    Ok(Output {
        exit_code,
        stdout: lossy(out),
        stderr: lossy(err),
        timed_out: false,
        truncated: out_cut || err_cut,
    })
}

async fn read_optional<R: AsyncRead + Unpin>(
    reader: Option<R>,
    cap: usize,
    buf: &mut Vec<u8>,
) -> bool {
    match reader {
        Some(reader) => read_capped(reader, cap, buf).await,
        None => false,
    }
}

/// 讀到結束,只保留前 `cap` 個位元組。回傳有沒有丟掉東西。
///
/// 超過之後**繼續讀、丟掉**,而不是停止讀:停下來的話對方寫 pipe 會卡住,變成逾時,真正的結束碼就看不到了。
pub(crate) async fn read_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
    buf: &mut Vec<u8>,
) -> bool {
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return truncated,
            Ok(n) => n,
        };
        let room = cap.saturating_sub(buf.len());
        let read = chunk.get(..n).unwrap_or_default();
        let keep = read.get(..room.min(n)).unwrap_or_default();
        buf.extend_from_slice(keep);
        if keep.len() < n {
            truncated = true;
        }
    }
}

fn lossy(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

/// 放進錯誤訊息的 stderr:控制字元換成空白、去掉頭尾空白、最多 `DETAIL_BYTES`(切在字元邊界)。
pub(crate) fn printable(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    let mut end = trimmed.len().min(DETAIL_BYTES);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed.get(..end).unwrap_or_default().to_owned()
}

/// 一次執行的容器。`remove()` 收掉;忘了或來不及(future 被取消)時,`Drop` 在背景收掉。
///
/// RAII:Java 要靠 try/finally 或 try-with-resources,而且 `CompletableFuture.cancel()` 不會執行 finally 裡的東西;
/// Rust 的 future 被 drop 時,它擁有的每個值都會被 drop,所以「被取消」也一定會走到這裡。
struct Container {
    program: String,
    name: String,
    removed: bool,
}

impl Container {
    fn new(program: &str, name: String) -> Self {
        Self {
            program: program.to_owned(),
            name,
            removed: false,
        }
    }

    fn remove_args(&self) -> [&str; 3] {
        ["rm", "--force", &self.name]
    }

    /// 收掉容器並等它完成(裡面還在跑的行程一起被 kill)。失敗也只能這樣:容器時間到會自己結束。
    async fn remove(mut self) {
        self.removed = true;
        let mut child = match Command::new(&self.program)
            .args(self.remove_args())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return,
        };
        let _ = tokio::time::timeout(REMOVE_TIMEOUT, child.wait()).await;
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        // `Drop` 不能 `.await`:有 tokio runtime 就丟一個背景 task 去收(tokio 會等子行程、不留殭屍);
        // 沒有(runtime 正在關)就只啟動 `docker rm`、不等它。
        let mut rm = std::process::Command::new(&self.program);
        rm.args(self.remove_args())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let mut rm = Command::from(rm);
                handle.spawn(async move {
                    if let Ok(mut child) = rm.spawn() {
                        let _ = tokio::time::timeout(REMOVE_TIMEOUT, child.wait()).await;
                    }
                });
            }
            Err(_) => {
                let _ = rm.spawn();
            }
        }
    }
}

#[cfg(test)]
mod tests;
