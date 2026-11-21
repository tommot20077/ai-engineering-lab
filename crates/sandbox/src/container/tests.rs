use super::*;

const DIGEST: &str = "sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7";

fn config_with(image: &str, limits: Limits) -> ContainerConfig {
    ContainerConfig {
        program: DEFAULT_PROGRAM.to_owned(),
        image: image.to_owned(),
        limits,
    }
}

fn invalid(config: ContainerConfig) -> String {
    match ContainerSandbox::new(config) {
        Err(SandboxError::InvalidConfig(reason)) => reason,
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
}

// ---- 設定的檢查 ----

#[test]
fn the_default_config_is_valid_and_pins_the_image_by_digest() {
    let sandbox = ContainerSandbox::new(ContainerConfig::default()).unwrap();
    assert_eq!(sandbox.config().program, "docker");
    assert_eq!(
        sandbox.config().image,
        format!("docker.io/library/rust:1.99.1-slim@{DIGEST}")
    );
}

#[test]
fn every_way_of_writing_a_pinned_image_is_accepted() {
    for image in [
        format!("rust@{DIGEST}"),
        format!("rust:1.99.1-slim@{DIGEST}"),
        format!("registry.example.com:5000/team/rust:1.99.1@{DIGEST}"),
    ] {
        let limits = Limits {
            pids: 7,
            ..Limits::default()
        };
        let sandbox = ContainerSandbox::new(config_with(&image, limits.clone())).unwrap();
        // 設定原樣保留(不是換成預設值)。
        assert_eq!(sandbox.config(), &config_with(&image, limits));
    }
}

#[test]
fn an_image_that_is_not_pinned_by_digest_is_rejected() {
    let reason = invalid(config_with("rust:1.99.1-slim", Limits::default()));
    assert_eq!(
        reason,
        "the image `rust:1.99.1-slim` must be pinned by digest (`name@sha256:<64 hex digits>`)"
    );
    for image in [
        "rust:1.99.1-slim@sha256:f6c4".to_owned(),  // 太短
        format!("rust@sha512:{}", "a".repeat(64)),  // 不是 sha256
        format!("rust@sha256:{}", "A".repeat(64)),  // 大寫
        format!("rust@sha256:{}g", "a".repeat(63)), // 不是 hex
        format!("rust@sha256:{}", "a".repeat(65)),  // 太長
        format!("@{DIGEST}"),                       // 沒有名稱
        format!("rust@{DIGEST} --privileged"),      // 夾帶其他東西
    ] {
        invalid(config_with(&image, Limits::default()));
    }
}

#[test]
fn an_empty_program_is_rejected() {
    let config = ContainerConfig {
        program: String::new(),
        ..ContainerConfig::default()
    };
    assert_eq!(invalid(config), "the container program must not be empty");
}

#[test]
fn every_limit_must_be_positive() {
    let image = DEFAULT_IMAGE;
    type Zero = fn(&mut Limits);
    let cases: [(&str, Zero); 7] = [
        ("compile_timeout", |l| l.compile_timeout = Duration::ZERO),
        ("run_timeout", |l| l.run_timeout = Duration::ZERO),
        ("memory_bytes", |l| l.memory_bytes = 0),
        ("pids", |l| l.pids = 0),
        ("output_bytes", |l| l.output_bytes = 0),
        ("scratch_bytes", |l| l.scratch_bytes = 0),
        ("code_bytes", |l| l.code_bytes = 0),
    ];
    for (name, zero) in cases {
        let mut limits = Limits::default();
        zero(&mut limits);
        assert_eq!(
            invalid(config_with(image, limits)),
            format!("the sandbox limit `{name}` must be greater than 0")
        );
    }
}

// ---- 參數:一個一個鎖住(改了任何一個隔離參數,這裡就會失敗,要在 review 時被看見)----

#[test]
fn the_container_starts_isolated() {
    let limits = Limits {
        compile_timeout: Duration::from_secs(20),
        run_timeout: Duration::from_secs(5),
        memory_bytes: 512 * 1024 * 1024,
        pids: 64,
        output_bytes: 1,
        scratch_bytes: 64 * 1024 * 1024,
        code_bytes: 1,
    };
    let args = start_args("rust@sha256:abc", &limits, "ai-lab-sandbox-x");
    let expected = [
        "run",
        "--detach",
        "--rm",
        "--name",
        "ai-lab-sandbox-x",
        "--label",
        "ai-engineering-lab.sandbox=1",
        "--pull",
        "never",
        "--network",
        "none",
        "--read-only",
        "--tmpfs",
        "/work:rw,exec,nosuid,nodev,size=67108864,uid=65534,gid=65534,mode=0700",
        "--workdir",
        "/work",
        "--env",
        "TMPDIR=/work",
        "--user",
        "65534:65534",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--memory",
        "536870912",
        "--memory-swap",
        "536870912",
        "--pids-limit",
        "64",
        "--cpus",
        "1",
        "--log-driver",
        "none",
        "rust@sha256:abc",
        "sleep",
        "125",
    ];
    assert_eq!(args, expected);
}

#[test]
fn the_container_sleeps_long_enough_for_every_step_and_then_exits_on_its_own() {
    // 啟動 60 + 寫入 10 + 編譯 + 執行 + 寬限 30:我們的程式被 SIGKILL、來不及收掉容器時,它自己結束,`--rm` 把它刪掉。
    let mut limits = Limits::default();
    assert_eq!(sleep_secs(&limits), 60 + 10 + 20 + 5 + 30);
    limits.compile_timeout = Duration::from_millis(1500);
    limits.run_timeout = Duration::from_millis(1);
    // 不足一秒的部分無條件進位,不能比上限短。
    assert_eq!(sleep_secs(&limits), 60 + 10 + 2 + 1 + 30);
}

#[test]
fn the_steps_inside_the_container() {
    // 原始碼從 stdin 進來:不經過命令列,內容不會被 shell 解讀。
    assert_eq!(
        write_args("c"),
        [
            "exec",
            "--interactive",
            "c",
            "sh",
            "-c",
            "cat > /work/main.rs"
        ]
    );
    assert_eq!(
        compile_args("c"),
        [
            "exec",
            "c",
            "rustc",
            "--edition",
            "2024",
            "--crate-name",
            "main",
            "-o",
            "/work/main",
            "/work/main.rs"
        ]
    );
    assert_eq!(exec_args("c"), ["exec", "c", "/work/main"]);
}

// ---- 讀輸出的上限 ----

#[tokio::test]
async fn output_below_the_limit_is_kept_whole() {
    let mut buf = Vec::new();
    let truncated = read_capped(&b"hello"[..], 10, &mut buf).await;
    assert_eq!((buf.as_slice(), truncated), (&b"hello"[..], false));
}

#[tokio::test]
async fn output_exactly_at_the_limit_is_not_truncated() {
    let mut buf = Vec::new();
    let truncated = read_capped(&b"hello"[..], 5, &mut buf).await;
    assert_eq!((buf.as_slice(), truncated), (&b"hello"[..], false));
}

#[tokio::test]
async fn output_over_the_limit_keeps_the_beginning_and_reads_the_rest() {
    // 比內部的讀取緩衝(8 KiB)大很多:確認跨多次讀取也只留開頭、而且讀到結束(不讀完的話,程式會卡在寫 pipe)。
    let data = vec![b'x'; 100_000];
    let mut reader = &data[..];
    let mut buf = Vec::new();
    let truncated = read_capped(&mut reader, 10_000, &mut buf).await;
    assert_eq!((buf.len(), truncated), (10_000, true));
    assert!(reader.is_empty(), "the rest must be drained");
}

// ---- 錯誤訊息裡的 stderr ----

#[test]
fn printable_replaces_control_characters_and_trims() {
    assert_eq!(
        printable(b"  Error: no\x1b[31m such\timage\r\n"),
        "Error: no [31m such image"
    );
}

#[test]
fn printable_keeps_at_most_1024_bytes_on_a_char_boundary() {
    let long = "é".repeat(600); // 1200 bytes,第 1024 個位元組剛好是邊界
    let out = printable(long.as_bytes());
    assert_eq!(out.len(), 1024);
    assert!(out.chars().all(|c| c == 'é'));
    // 前面多一個 a:第 1024 個位元組落在 é 的中間,要退到 1023。
    let shifted = format!("a{long}");
    let out = printable(shifted.as_bytes());
    assert_eq!(out.len(), 1023);
    assert!(out.starts_with("aé") && out.ends_with('é'));
}

// ---- 不必啟動容器就能判斷的情況 ----

#[tokio::test]
async fn code_over_the_limit_is_rejected_before_anything_runs() {
    // program 不存在:如果真的去執行,會得到 Unavailable 而不是 TooLarge。
    let config = ContainerConfig {
        program: "/nonexistent/docker".to_owned(),
        limits: Limits {
            code_bytes: 4,
            ..Limits::default()
        },
        ..ContainerConfig::default()
    };
    let sandbox = ContainerSandbox::new(config).unwrap();
    assert_eq!(
        sandbox.rust("fn m", Action::Run).await,
        Err(SandboxError::Unavailable {
            program: "/nonexistent/docker".to_owned(),
            reason: "No such file or directory (os error 2)".to_owned(),
        }),
        "exactly at the limit is allowed",
    );
    assert_eq!(
        sandbox.rust("fn ma", Action::Run).await,
        Err(SandboxError::TooLarge { limit: 4 })
    );
}
