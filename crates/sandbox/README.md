# sandbox

在隔離的容器裡編譯、執行**不可信的**單檔 Rust。agent 的 `run_rust` 工具與 eval 的 `compiles` 檢查都用它;
這個 crate 不知道它們存在,它們只看得到 `Sandbox` trait。設計與取捨見
[`design-part3.md`](../../docs/modules/003-agent-runtime/design-part3.md)(D-053、D-054)。

**編譯也是執行**:`include_str!("/home/me/.ssh/id_ed25519")` 在編譯時就讀檔,所以編譯也在沙箱裡。

```
ContainerSandbox::rust(source, Action::Run)
  docker run --detach … image sleep N   一次一個新容器;隔離參數全部在這裡
  docker exec --interactive … cat       原始碼從 stdin 寫進 /work/main.rs(不經過命令列)
  docker exec … rustc --edition 2024    編譯(主機計時,20 秒)
  docker exec … /work/main              執行(主機計時,5 秒)
  docker rm --force                     不管成功、失敗、逾時、被取消,一定收掉
```

## 快速開始

要 Docker(或 Podman)。image 是官方的 `rust:1.99.1-slim`,用 digest 鎖定;沙箱**不會自動下載**(1 GB 以上的下載
不該在工具呼叫裡默默發生),第一次先拉:

```bash
docker pull "$(cargo run -q -p sandbox --example run_rust -- --image)"
cargo run -p sandbox --example run_rust -- path/to/main.rs
cargo run -p sandbox --example run_rust -- --compile-only path/to/main.rs
```

## 當 library 用

```rust
use sandbox::{Action, ContainerConfig, ContainerSandbox, Sandbox};

let sandbox = ContainerSandbox::new(ContainerConfig::default())?;   // image 沒用 digest 鎖定、上限是 0 → 這裡就失敗
let report = sandbox.rust(source, Action::Run).await?;              // Err 只代表沙箱本身不能用
if report.compile.success() {
    let run = report.run.unwrap();       // exit_code、stdout、stderr、timed_out、truncated
}
```

程式自己的失敗(編譯錯誤、結束碼不是 0、逾時、輸出太多)都在 `Report` 裡,不是 `Err`。

## 隔離與上限

| 威脅 | 怎麼擋 | 逃逸測試 |
|---|---|---|
| 把資料送出去 | `--network none`(只有 `lo`) | `there_is_no_network` |
| 讀主機的檔案 | 容器裡只有 image 的檔案;編譯也在容器裡 | `include_str_cannot_read_host_files_at_compile_time` |
| 讀秘密 | 容器不繼承主機的環境變數 | `host_files_and_environment_are_not_visible` |
| 改系統檔案 | `--read-only`;唯一能寫的是 tmpfs `/work`(64 MiB) | `only_the_work_directory_is_writable`、`the_work_directory_is_capped` |
| 提權 | nobody(65534)、`--cap-drop ALL`、`no-new-privileges` | `the_program_runs_as_nobody_without_capabilities` |
| fork bomb | `--pids-limit 64` | `a_fork_bomb_hits_the_process_limit` |
| 吃光記憶體 | `--memory 512m`,`--memory-swap` 一樣大(不能用 swap 繞過) | `memory_is_capped` |
| 無窮迴圈 | 主機計時:編譯 20 秒、執行 5 秒,時間到收掉整個容器 | `an_infinite_loop_is_stopped_at_the_time_limit` |
| 輸出幾 GB | stdout、stderr 各留 64 KiB,其餘讀掉丟棄 | `huge_output_is_truncated` |
| 留下背景行程 | 一次一個容器,結束就 `rm --force`;我們被 SIGKILL 時容器的 `sleep` 到期自己結束(`--rm`) | `background_processes_do_not_outlive_the_run` |
| image 被換掉 | 只收 `name@sha256:<64 hex>` | 單元測試 |

上限都可以調(`Limits`),不能關掉(0 會被拒絕)。逾時由**主機**判斷:容器裡的 `timeout` 跟不可信的程式是同一個使用者,可以被 kill 掉。

## 測試

| 層 | 檔案 | 內容 |
|---|---|---|
| 單元 | `src/container/tests.rs` | `docker run` 的每個參數(一個一個鎖住)、image 與上限的檢查、讀輸出的上限、錯誤訊息的整理 |
| 整合(假的 docker) | `tests/fake_docker.rs` | 用 shell script 扮演 docker:步驟順序、逾時保留已讀的輸出、上限、一定收掉容器(包括被取消時)。不需要 docker,每次都跑 |
| 整合(真的容器) | `tests/escape.rs` | 上表的每一項。`SANDBOX_DOCKER=1` 才跑;**設了卻跑不起來就失敗**(CI 設它) |

```bash
SANDBOX_DOCKER=1 cargo test -p sandbox --test escape
```

一次只跑一個容器(測試之間用一把鎖),逾時才不會被別的測試搶 CPU 影響,而且每次之後可以直接確認「沒有留下任何沙箱容器」
(容器都貼了 label `ai-engineering-lab.sandbox=1`)。

## 原始碼導覽

| 檔案 | 內容 |
|---|---|
| `src/lib.rs` | `Sandbox` trait、`Action`、`Limits`、`Output`、`Report` |
| `src/container.rs` | `ContainerSandbox`:檢查設定、組參數、四個步驟、讀輸出、`Container` 守衛(RAII 收掉容器) |
| `src/error.rs` | `SandboxError` |
| `examples/run_rust.rs` | 編譯並執行一個檔案;`--image` 印出鎖定的 image |
