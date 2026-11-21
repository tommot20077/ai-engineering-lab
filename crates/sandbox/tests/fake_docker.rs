//! 用一個假的 `docker`(shell script)測 `ContainerSandbox` 的流程:步驟的順序、逾時、上限、一定收掉容器。
//!
//! 不需要真的 docker,所以每次都跑(CI 的 Mutants (diff) 也跑得到)。隔離本身有沒有效,由 `escape.rs` 對真的容器測。
//! 假的 docker 依原始碼裡的標記決定怎麼表現(`COMPILE_FAIL`、`RUN_HANG`……),每次呼叫的參數記在 `calls`。
#![cfg(unix)]
// 測試的輔助函式(不在 #[test] 裡)也可以 unwrap / 索引:失敗就是測試失敗。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sandbox::{
    Action, ContainerConfig, ContainerSandbox, Limits, Output, Report, Sandbox, SandboxError,
};

const DIGEST: &str = "sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7";

const SCRIPT: &str = r#"#!/bin/sh
D='__DIR__'
echo "$*" >> "$D/calls"
case "$1" in
  run)
    for a in "$@"; do
      case "$a" in
        missing*) echo "Unable to find image '$a' locally" >&2; exit 125 ;;
        broken*) printf 'Cannot connect to the Docker daemon\033[0m\n' >&2; exit 1 ;;
      esac
    done
    echo 0123456789abcdef ;;
  rm) ;;
  exec)
    if [ "$2" = "--interactive" ]; then
      cat > "$D/main.rs"
      grep -q WRITE_FAIL "$D/main.rs" && { echo "no space left" >&2; exit 1; }
      exit 0
    fi
    case "$3" in
      rustc)
        grep -q COMPILE_FAIL "$D/main.rs" && { echo "error[E0425]: cannot find value" >&2; exit 1; }
        grep -q COMPILE_HANG "$D/main.rs" && { echo compiling; exec sleep 30; }
        exit 0 ;;
      /work/main)
        grep -q RUN_HANG "$D/main.rs" && { echo partial; exec sleep 30; }
        grep -q RUN_BIG "$D/main.rs" && { head -c 300000 /dev/zero | tr '\000' 'x'; exit 0; }
        grep -q RUN_EXIT3 "$D/main.rs" && { echo out; echo err >&2; exit 3; }
        echo hello
        exit 0 ;;
    esac ;;
esac
"#;

struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = SCRIPT.replace("__DIR__", dir.path().to_str().unwrap());
        let path = dir.path().join("docker");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // 測試是平行跑的:別的測試執行緒剛好在 fork 時,子行程(exec 之前)會短暫繼承我們寫檔用的 fd,
        // 這時執行這個 script 會得到 ETXTBSY(「Text file busy」)。先試著執行到成功為止:成功的那一刻沒有人開著它寫,
        // 之後也不會再有(我們已經關掉了)。
        loop {
            match std::process::Command::new(&path).arg("probe").status() {
                Err(e) if e.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                other => {
                    other.unwrap();
                    break;
                }
            }
        }
        std::fs::remove_file(dir.path().join("calls")).unwrap();
        Self { dir }
    }

    fn program(&self) -> PathBuf {
        self.dir.path().join("docker")
    }

    fn sandbox(&self, limits: Limits) -> ContainerSandbox {
        self.sandbox_with_image(&format!("rust@{DIGEST}"), limits)
    }

    fn sandbox_with_image(&self, image: &str, limits: Limits) -> ContainerSandbox {
        ContainerSandbox::new(ContainerConfig {
            program: self.program().to_str().unwrap().to_owned(),
            image: image.to_owned(),
            limits,
        })
        .unwrap()
    }

    /// 每次呼叫 docker 的參數,一行一次。
    fn calls(&self) -> Vec<String> {
        read_lines(&self.dir.path().join("calls"))
    }

    fn source(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("main.rs")).unwrap()
    }
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// 呼叫的第一個字(`run`、`exec`、`rm`)與它操作的容器名稱。
fn verbs(calls: &[String]) -> Vec<String> {
    calls
        .iter()
        .map(|c| {
            let words: Vec<&str> = c.split(' ').collect();
            match words[0] {
                "run" => "run".to_owned(),
                "rm" => format!("rm {}", words[words.len() - 1]),
                "exec" if words[1] == "--interactive" => "write".to_owned(),
                "exec" => format!("exec {}", words[2]),
                other => other.to_owned(),
            }
        })
        .collect()
}

fn name_of(calls: &[String]) -> String {
    let words: Vec<&str> = calls[0].split(' ').collect();
    let at = words.iter().position(|w| *w == "--name").unwrap();
    words[at + 1].to_owned()
}

fn ok(stdout: &str) -> Output {
    Output {
        exit_code: Some(0),
        stdout: stdout.to_owned(),
        stderr: String::new(),
        timed_out: false,
        truncated: false,
    }
}

fn fast() -> Limits {
    Limits {
        compile_timeout: Duration::from_secs(5),
        run_timeout: Duration::from_secs(5),
        ..Limits::default()
    }
}

/// 錯的實作可能卡住:讓測試失敗而不是卡到 CI 逾時。
async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), f)
        .await
        .expect("the sandbox did not finish in time")
}

#[tokio::test]
async fn run_compiles_then_runs_then_removes_the_container() {
    let fake = Fake::new();
    let report = within(fake.sandbox(fast()).rust("fn main() {}", Action::Run))
        .await
        .unwrap();
    assert_eq!(
        report,
        Report {
            compile: ok(""),
            run: Some(ok("hello\n")),
        }
    );
    let calls = fake.calls();
    let name = name_of(&calls);
    assert!(name.starts_with("ai-lab-sandbox-"), "{name}");
    assert_eq!(
        verbs(&calls),
        [
            "run".to_owned(),
            "write".to_owned(),
            "exec rustc".to_owned(),
            "exec /work/main".to_owned(),
            format!("rm {name}")
        ]
    );
    // 每一步都對同一個容器。
    assert!(calls[1..4].iter().all(|c| c.contains(&name)), "{calls:?}");
    assert_eq!(calls[4], format!("rm --force {name}"));
    assert_eq!(fake.source(), "fn main() {}");
}

#[tokio::test]
async fn every_run_uses_a_new_container_name() {
    let fake = Fake::new();
    let sandbox = fake.sandbox(fast());
    within(sandbox.rust("fn main() {}", Action::Compile))
        .await
        .unwrap();
    let first = name_of(&fake.calls());
    std::fs::remove_file(fake.dir.path().join("calls")).unwrap();
    within(sandbox.rust("fn main() {}", Action::Compile))
        .await
        .unwrap();
    assert_ne!(first, name_of(&fake.calls()));
}

#[tokio::test]
async fn compile_only_does_not_run_the_program() {
    let fake = Fake::new();
    let report = within(fake.sandbox(fast()).rust("fn main() {}", Action::Compile))
        .await
        .unwrap();
    assert_eq!(
        report,
        Report {
            compile: ok(""),
            run: None
        }
    );
    let calls = fake.calls();
    assert_eq!(
        verbs(&calls),
        [
            "run".to_owned(),
            "write".to_owned(),
            "exec rustc".to_owned(),
            format!("rm {}", name_of(&calls))
        ]
    );
}

#[tokio::test]
async fn a_failed_compile_is_reported_and_nothing_runs() {
    let fake = Fake::new();
    let report = within(fake.sandbox(fast()).rust("COMPILE_FAIL", Action::Run))
        .await
        .unwrap();
    assert_eq!(
        report,
        Report {
            compile: Output {
                exit_code: Some(1),
                stdout: String::new(),
                stderr: "error[E0425]: cannot find value\n".to_owned(),
                timed_out: false,
                truncated: false,
            },
            run: None,
        }
    );
    assert!(!report.compile.success());
    assert!(!verbs(&fake.calls()).contains(&"exec /work/main".to_owned()));
}

#[tokio::test]
async fn the_exit_code_and_both_streams_of_the_program_are_reported() {
    let fake = Fake::new();
    let report = within(fake.sandbox(fast()).rust("RUN_EXIT3", Action::Run))
        .await
        .unwrap();
    assert_eq!(
        report.run,
        Some(Output {
            exit_code: Some(3),
            stdout: "out\n".to_owned(),
            stderr: "err\n".to_owned(),
            timed_out: false,
            truncated: false,
        })
    );
}

#[tokio::test]
async fn a_hanging_compile_times_out_keeps_its_output_and_removes_the_container() {
    let fake = Fake::new();
    let limits = Limits {
        compile_timeout: Duration::from_millis(500),
        ..fast()
    };
    let started = Instant::now();
    let report = within(fake.sandbox(limits).rust("COMPILE_HANG", Action::Run))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        report,
        Report {
            compile: Output {
                exit_code: None,
                stdout: "compiling\n".to_owned(),
                stderr: String::new(),
                timed_out: true,
                truncated: false,
            },
            run: None,
        }
    );
    let calls = fake.calls();
    assert_eq!(
        verbs(&calls).last().unwrap(),
        &format!("rm {}", name_of(&calls))
    );
}

#[tokio::test]
async fn a_hanging_program_times_out() {
    let fake = Fake::new();
    let limits = Limits {
        run_timeout: Duration::from_millis(500),
        ..fast()
    };
    let started = Instant::now();
    let report = within(fake.sandbox(limits).rust("RUN_HANG", Action::Run))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(report.compile.success());
    assert_eq!(
        report.run,
        Some(Output {
            exit_code: None,
            stdout: "partial\n".to_owned(),
            stderr: String::new(),
            timed_out: true,
            truncated: false,
        })
    );
    assert!(!report.run.unwrap().success());
}

#[tokio::test]
async fn the_compile_timeout_does_not_apply_to_running() {
    // 編譯上限 0.5 秒、執行上限 1.5 秒:卡住的程式要在 1.5 秒(而不是 0.5 秒)才被停下來。
    let fake = Fake::new();
    let limits = Limits {
        compile_timeout: Duration::from_millis(500),
        run_timeout: Duration::from_millis(1500),
        ..fast()
    };
    let started = Instant::now();
    let report = within(fake.sandbox(limits).rust("RUN_HANG", Action::Run))
        .await
        .unwrap();
    assert!(report.run.unwrap().timed_out);
    assert!(
        started.elapsed() >= Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn big_output_is_truncated_at_the_limit() {
    let fake = Fake::new();
    let limits = Limits {
        output_bytes: 1000,
        ..fast()
    };
    let report = within(fake.sandbox(limits).rust("RUN_BIG", Action::Run))
        .await
        .unwrap();
    assert_eq!(
        report.run,
        Some(Output {
            exit_code: Some(0),
            stdout: "x".repeat(1000),
            stderr: String::new(),
            timed_out: false,
            truncated: true,
        })
    );
}

#[tokio::test]
async fn a_missing_image_is_reported_with_the_pull_command() {
    let fake = Fake::new();
    let image = format!("missing@{DIGEST}");
    let err = within(
        fake.sandbox_with_image(&image, fast())
            .rust("fn main() {}", Action::Run),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        SandboxError::ImageMissing {
            program: fake.program().to_str().unwrap().to_owned(),
            image,
        }
    );
    // 沒有起來的容器也送一次 rm(不存在也沒關係),比「判斷要不要收」可靠。
    let calls = fake.calls();
    assert_eq!(
        verbs(&calls),
        ["run".to_owned(), format!("rm {}", name_of(&calls))]
    );
}

#[tokio::test]
async fn a_failed_start_is_reported_with_printable_stderr() {
    let fake = Fake::new();
    let image = format!("broken@{DIGEST}");
    let err = within(
        fake.sandbox_with_image(&image, fast())
            .rust("fn main() {}", Action::Run),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        SandboxError::Container {
            detail: "could not start the container: Cannot connect to the Docker daemon [0m"
                .to_owned()
        }
    );
}

#[tokio::test]
async fn a_failed_write_is_an_error_and_the_container_is_removed() {
    let fake = Fake::new();
    let err = within(fake.sandbox(fast()).rust("WRITE_FAIL", Action::Run))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        SandboxError::Container {
            detail: "could not write the source file: no space left".to_owned()
        }
    );
    let calls = fake.calls();
    assert_eq!(
        verbs(&calls),
        [
            "run".to_owned(),
            "write".to_owned(),
            format!("rm {}", name_of(&calls))
        ]
    );
}

#[tokio::test]
async fn a_cancelled_run_still_removes_the_container() {
    // 呼叫端不等了(例如 agent 的總時間到了):future 被 drop,容器也要被收掉。
    let fake = Fake::new();
    let sandbox = fake.sandbox(fast());
    let cancelled = tokio::time::timeout(
        Duration::from_millis(500),
        sandbox.rust("RUN_HANG", Action::Run),
    )
    .await;
    assert!(cancelled.is_err(), "the run should still be going");
    // 收掉容器是在背景做的:等它出現在紀錄裡。
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let calls = fake.calls();
        if verbs(&calls).last() == Some(&format!("rm {}", name_of(&calls))) {
            break;
        }
        assert!(Instant::now() < deadline, "no rm after cancel: {calls:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
