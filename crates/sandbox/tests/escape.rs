//! 逃逸測試:對**真的容器**執行惡意或失控的程式,每一項都要被擋下或被上限停住(design-part3 §6)。
//!
//! 只在 `SANDBOX_DOCKER=1` 時跑(要 docker 與預設的 image:`docker pull` 見 README)。
//! 設了卻跑不起來就**失敗**,不是略過:CI 設了它,壞掉的沙箱不能被當成「沒有 docker」默默通過。
//!
//! 每個測試先拿同一把鎖:一次只跑一個容器。逾時的判斷才不會被別的測試搶 CPU 影響,
//! 而且「跑完之後沒有留下任何沙箱容器」可以直接數。
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::process::Command;
use std::time::{Duration, Instant};

use sandbox::{Action, ContainerConfig, ContainerSandbox, Limits, Report, Sandbox};
use tokio::sync::{Mutex, MutexGuard};

static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());

const LABEL: &str = "label=ai-engineering-lab.sandbox=1";

fn enabled() -> bool {
    // 讀環境變數是安全的(只有 `set_var` 是 unsafe)。
    std::env::var("SANDBOX_DOCKER").is_ok_and(|v| v == "1")
}

struct Docker {
    sandbox: ContainerSandbox,
    _turn: MutexGuard<'static, ()>,
}

/// `None`:沒有設 `SANDBOX_DOCKER=1`,略過。
async fn docker(limits: Limits) -> Option<Docker> {
    if !enabled() {
        eprintln!("skipped: set SANDBOX_DOCKER=1 to run the escape tests against a real container");
        return None;
    }
    let turn = ONE_AT_A_TIME.lock().await;
    let sandbox = ContainerSandbox::new(ContainerConfig {
        limits,
        ..ContainerConfig::default()
    })
    .unwrap();
    Some(Docker {
        sandbox,
        _turn: turn,
    })
}

impl Docker {
    async fn run(&self, source: &str) -> Report {
        let report = self.sandbox.rust(source, Action::Run).await.unwrap();
        // 每一次之後:沒有留下任何沙箱容器(背景行程也跟著容器消失)。
        assert_eq!(
            sandbox_containers(),
            "",
            "a sandbox container was left behind"
        );
        report
    }

    /// 編譯要成功,回傳執行的 stdout。
    async fn stdout(&self, source: &str) -> String {
        let report = self.run(source).await;
        assert!(report.compile.success(), "compile failed: {report:?}");
        report.run.unwrap().stdout
    }
}

fn sandbox_containers() -> String {
    let out = Command::new("docker")
        .args(["ps", "--all", "--quiet", "--filter", LABEL])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

fn fast() -> Limits {
    Limits {
        run_timeout: Duration::from_secs(3),
        ..Limits::default()
    }
}

#[tokio::test]
async fn a_normal_program_compiles_and_runs() {
    let Some(d) = docker(fast()).await else {
        return;
    };
    let report = d
        .run(r#"fn main() { println!("hello"); eprintln!("bye"); std::process::exit(3) }"#)
        .await;
    assert!(report.compile.success(), "{report:?}");
    let run = report.run.unwrap();
    assert_eq!(
        (run.exit_code, run.stdout.as_str(), run.stderr.as_str()),
        (Some(3), "hello\n", "bye\n")
    );
}

#[tokio::test]
async fn compile_errors_come_back_in_stderr() {
    let Some(d) = docker(fast()).await else {
        return;
    };
    let report = d.run("fn main() { let x: u8 = \"no\"; }").await;
    assert_eq!(report.compile.exit_code, Some(1));
    assert!(
        report
            .compile
            .stderr
            .contains("error[E0308]: mismatched types"),
        "{report:?}"
    );
    assert_eq!(report.run, None);
}

#[tokio::test]
async fn the_program_runs_as_nobody_without_capabilities() {
    // 不是 root 時 CapEff、CapPrm 本來就是 0;CapBnd(上限)只有 `--cap-drop ALL` 才會是 0,
    // 它擋的是「之後透過 setuid 程式變回 root 也拿不到 capability」。
    let Some(d) = docker(fast()).await else {
        return;
    };
    let out = d
        .stdout(
            r#"fn main() {
                let s = std::fs::read_to_string("/proc/self/status").unwrap();
                for line in s.lines() {
                    if line.starts_with("Uid:") || line.starts_with("Gid:")
                        || line.starts_with("CapEff:") || line.starts_with("CapPrm:")
                        || line.starts_with("CapBnd:")
                        || line.starts_with("NoNewPrivs:") {
                        println!("{}", line.split_whitespace().collect::<Vec<_>>().join(" "));
                    }
                }
            }"#,
        )
        .await;
    assert_eq!(
        out,
        "Uid: 65534 65534 65534 65534\nGid: 65534 65534 65534 65534\n\
         CapPrm: 0000000000000000\nCapEff: 0000000000000000\nCapBnd: 0000000000000000\nNoNewPrivs: 1\n"
    );
}

#[tokio::test]
async fn there_is_no_network() {
    let Some(d) = docker(fast()).await else {
        return;
    };
    let out = d
        .stdout(
            r#"use std::net::{TcpStream, ToSocketAddrs, SocketAddr};
            fn main() {
                let ip: SocketAddr = "1.1.1.1:443".parse().unwrap();
                match TcpStream::connect_timeout(&ip, std::time::Duration::from_secs(1)) {
                    Ok(_) => println!("tcp: connected"),
                    Err(e) => println!("tcp: {:?}", e.kind()),
                }
                match "example.com:443".to_socket_addrs() {
                    Ok(_) => println!("dns: resolved"),
                    Err(_) => println!("dns: failed"),
                }
                let ifaces = std::fs::read_to_string("/proc/net/dev").unwrap();
                let names: Vec<&str> = ifaces.lines().skip(2)
                    .map(|l| l.split(':').next().unwrap().trim()).collect();
                println!("interfaces: {names:?}");
            }"#,
        )
        .await;
    assert_eq!(
        out,
        "tcp: NetworkUnreachable\ndns: failed\ninterfaces: [\"lo\"]\n"
    );
}

#[tokio::test]
async fn only_the_work_directory_is_writable() {
    let Some(d) = docker(fast()).await else {
        return;
    };
    let out = d
        .stdout(
            r#"fn main() {
                for path in ["/evil", "/tmp/evil", "/etc/passwd", "/usr/local/cargo/bin/rustc", "/work/ok"] {
                    match std::fs::write(path, b"x") {
                        Ok(()) => println!("{path}: written"),
                        Err(e) => println!("{path}: {:?}", e.kind()),
                    }
                }
                // 程式自己要暫存檔時(`std::env::temp_dir()`),要落在可寫的 /work(TMPDIR),而不是唯讀的 /tmp。
                let tmp = std::env::temp_dir().join("scratch");
                match std::fs::write(&tmp, b"x") {
                    Ok(()) => println!("{}: written", tmp.display()),
                    Err(e) => println!("{}: {:?}", tmp.display(), e.kind()),
                }
            }"#,
        )
        .await;
    assert_eq!(
        out,
        "/evil: ReadOnlyFilesystem\n/tmp/evil: ReadOnlyFilesystem\n/etc/passwd: ReadOnlyFilesystem\n\
         /usr/local/cargo/bin/rustc: ReadOnlyFilesystem\n/work/ok: written\n/work/scratch: written\n"
    );
}

#[tokio::test]
async fn host_files_and_environment_are_not_visible() {
    let Some(d) = docker(fast()).await else {
        return;
    };
    // 主機上的秘密檔案:在容器裡不存在。
    let secret = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(secret.path(), "TOP-SECRET-MARKER").unwrap();
    let path = secret.path().to_str().unwrap();
    let source = format!(
        r#"fn main() {{
            println!("file: {{}}", std::path::Path::new("{path}").exists());
            let mut names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
            names.sort();
            println!("env: {{}}", names.join(","));
        }}"#
    );
    let out = d.stdout(&source).await;
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("file: false"));
    // 只有 image 自己的與我們設的;主機的(例如 cargo 給測試的 CARGO_*、可能有的 API key)一個都不在。
    let env = lines.next().unwrap().strip_prefix("env: ").unwrap();
    let allowed = [
        "CARGO_HOME",
        "HOME",
        "HOSTNAME",
        "PATH",
        "RUSTUP_HOME",
        "RUST_VERSION",
        "TMPDIR",
    ];
    for name in env.split(',') {
        assert!(
            allowed.contains(&name),
            "unexpected variable in the sandbox: {name}"
        );
    }
}

#[tokio::test]
async fn include_str_cannot_read_host_files_at_compile_time() {
    let Some(d) = docker(fast()).await else {
        return;
    };
    let secret = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(secret.path(), "TOP-SECRET-MARKER").unwrap();
    let path = secret.path().to_str().unwrap();
    let report = d
        .run(&format!(
            r#"fn main() {{ println!("{{}}", include_str!("{path}")); }}"#
        ))
        .await;
    assert_eq!(report.compile.exit_code, Some(1), "{report:?}");
    assert!(
        report.compile.stderr.contains("couldn't read"),
        "{report:?}"
    );
    assert!(!report.compile.stderr.contains("TOP-SECRET-MARKER"));
    assert_eq!(report.run, None);
}

#[tokio::test]
async fn a_fork_bomb_hits_the_process_limit() {
    let Some(d) = docker(fast()).await else {
        return;
    };
    let out = d
        .stdout(
            r#"fn main() {
                let mut started = 0;
                for _ in 0..1000 {
                    match std::process::Command::new("sleep").arg("30").spawn() {
                        Ok(_) => started += 1,
                        Err(_) => break,
                    }
                }
                println!("{started}");
            }"#,
        )
        .await;
    let started: u32 = out.trim().parse().unwrap();
    // 64 個裡面:容器的 PID 1(sleep)與這個程式自己各佔一個。
    assert!((1..64).contains(&started), "started {started} processes");
}

#[tokio::test]
async fn an_infinite_loop_is_stopped_at_the_time_limit() {
    let Some(d) = docker(Limits {
        run_timeout: Duration::from_secs(2),
        ..Limits::default()
    })
    .await
    else {
        return;
    };
    let source = r#"fn main() { println!("start"); loop { std::hint::black_box(()); } }"#;
    let started = Instant::now();
    let report = d.run(source).await;
    let run = report.run.unwrap();
    assert!(run.timed_out, "{run:?}");
    assert_eq!((run.exit_code, run.stdout.as_str()), (None, "start\n"));
    // 編譯 + 2 秒 + 收掉容器;遠小於編譯上限 20 秒。
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_slow_compile_is_stopped_at_the_compile_limit() {
    // 編譯本身就停不下來:常數在**編譯時**計算,迴圈要跑 2^64 次。`long_running_const_eval` 預設會擋下這種程式,
    // 允許它之後 rustc 就一直算下去,只能靠我們的上限停住。
    // (原本用「上限 0.1 秒編譯 `fn main() {}`」,在 CI 的機器上 0.1 秒內就編譯完了:測試的前提不能依賴機器快慢。)
    let Some(d) = docker(Limits {
        compile_timeout: Duration::from_secs(2),
        ..Limits::default()
    })
    .await
    else {
        return;
    };
    let source = r#"#![allow(long_running_const_eval)]
        const SPIN: u64 = { let mut i: u64 = 0; while i < u64::MAX { i += 1; } i };
        fn main() { println!("{SPIN}"); }"#;
    let started = Instant::now();
    let report = d.run(source).await;
    assert!(report.compile.timed_out, "{report:?}");
    assert_eq!((report.compile.exit_code, report.run), (None, None));
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn memory_is_capped() {
    let Some(d) = docker(fast()).await else {
        return;
    };
    // 配置 1 GiB 並且真的寫入每一頁(只配置不寫,Linux 不會真的給記憶體)。
    let report = d
        .run(
            r#"fn main() {
                let mut v = vec![0u8; 1 << 30];
                for i in (0..v.len()).step_by(4096) { v[i] = 1; }
                println!("survived {}", v.len());
            }"#,
        )
        .await;
    let run = report.run.unwrap();
    assert!(!run.success(), "{run:?}");
    assert_eq!(run.stdout, "");
    // OOM killer 送 SIGKILL:128 + 9。
    assert_eq!(run.exit_code, Some(137), "{run:?}");
}

#[tokio::test]
async fn the_work_directory_is_capped() {
    let Some(d) = docker(fast()).await else {
        return;
    };
    let out = d
        .stdout(
            r#"use std::io::Write;
            fn main() {
                let mut f = std::fs::File::create("/work/fill").unwrap();
                let chunk = vec![0u8; 1 << 20];
                let mut written = 0usize;
                loop {
                    match f.write_all(&chunk) {
                        Ok(()) => written += 1,
                        Err(e) => { println!("{written} {:?}", e.kind()); break; }
                    }
                }
            }"#,
        )
        .await;
    let mut parts = out.split_whitespace();
    let written: usize = parts.next().unwrap().parse().unwrap();
    // 64 MiB 裡面還有原始碼與編譯出來的執行檔。
    assert!(written < 64, "wrote {written} MiB");
    assert_eq!(parts.next(), Some("StorageFull"));
}

#[tokio::test]
async fn huge_output_is_truncated() {
    let Some(d) = docker(fast()).await else {
        return;
    };
    let report = d
        .run(r#"fn main() { let line = "x".repeat(1023); for _ in 0..10_240 { println!("{line}"); } }"#)
        .await;
    let run = report.run.unwrap();
    assert!(
        run.truncated && run.success(),
        "{:?}",
        (run.truncated, run.exit_code)
    );
    assert_eq!(run.stdout.len(), 64 * 1024);
}

#[tokio::test]
async fn background_processes_do_not_outlive_the_run() {
    let Some(d) = docker(fast()).await else {
        return;
    };
    // 留下一個背景行程就結束。`run` 會檢查之後沒有任何沙箱容器(行程只能活在容器裡,容器沒了它就沒了)。
    let out = d
        .stdout(
            r#"fn main() {
                let child = std::process::Command::new("sleep").arg("1000")
                    .stdout(std::process::Stdio::null()).spawn().unwrap();
                println!("left {}", child.id() > 0);
            }"#,
        )
        .await;
    assert_eq!(out, "left true\n");
}
