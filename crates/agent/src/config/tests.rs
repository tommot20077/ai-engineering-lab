use std::path::{Path, PathBuf};

use llm::{ProviderMetadata, Response, StopKind, StopReason};

use super::*;

fn parse(text: &str) -> ConfigFile<RawConfig> {
    ConfigFile {
        value: toml::from_str(text).unwrap(),
        dir: PathBuf::from("/conf"),
    }
}

fn fake_data_dir(app: &str) -> config_loader::Result<PathBuf> {
    Ok(Path::new("/data").join(app))
}

fn settings(text: &str) -> Result<Settings, InvalidConfig> {
    Settings::from_config(&parse(text), fake_data_dir)
}

const MINIMAL: &str = r#"
[aliases]
default = "claude-sonnet-5-5"
"#;

#[test]
fn a_minimal_config_gets_defaults() {
    let s = settings(MINIMAL).unwrap();
    assert_eq!(s.base_url, DEFAULT_BASE_URL);
    assert_eq!(s.model, "default");
    assert_eq!(s.max_tokens, 4096);
    assert_eq!(s.limits, AgentLimits::default());
    assert_eq!(s.runs_dir, PathBuf::from("/data/agent/runs"));
    // 呼叫紀錄預設跟 gateway 寫在同一個地方。
    assert_eq!(s.records_dir, PathBuf::from("/data/gateway/records"));
    assert!(s.prices.is_empty());
}

#[test]
fn every_section_can_be_set() {
    let s = settings(
        r#"
[aliases]
default = "claude-sonnet-5-5"
cheap = "claude-haiku-5-5"
[anthropic]
base_url = "http://localhost:9999"
[agent]
model = "cheap"
max_tokens = 2000
[limits]
max_turns = 5
tool_timeout_secs = 10
[paths]
data_dir = "data"
records = "/var/records"
[prices."claude-haiku-5-5"]
input = 0.1
output = 0.5
"#,
    )
    .unwrap();
    assert_eq!(s.base_url, "http://localhost:9999");
    assert_eq!(s.model, "cheap");
    assert_eq!(s.max_tokens, 2000);
    // 只覆寫寫出來的欄位,其他維持預設。
    assert_eq!(
        s.limits,
        AgentLimits {
            max_turns: 5,
            tool_timeout_secs: 10,
            ..AgentLimits::default()
        }
    );
    // 相對路徑以設定檔所在的目錄為準。
    assert_eq!(s.runs_dir, PathBuf::from("/conf/data/runs"));
    assert_eq!(s.records_dir, PathBuf::from("/var/records"));
    assert_eq!(
        s.prices["claude-haiku-5-5"],
        Price {
            input: 0.1,
            output: 0.5
        }
    );
}

#[test]
fn every_problem_is_listed_at_once() {
    let error = settings(
        r#"
[aliases]
cheap = "claude-haiku-5-5"
blank = " "
[anthropic]
base_url = "http://example.com"
[agent]
model = "default"
max_tokens = 0
[limits]
max_turns = 0
max_parallel = 0
[prices.cheap]
input = 1.0
output = -1.0
"#,
    )
    .err()
    .unwrap();
    assert_eq!(
        error.0,
        [
            "[aliases] `blank` must map a non-empty name to a non-empty model id",
            "anthropic.base_url `http://example.com` must use https (plain http is only allowed for localhost)",
            "agent.model `default` is not defined in [aliases]",
            "agent.max_tokens must be at least 1",
            "limits.max_turns must be at least 1",
            "limits.max_parallel must be at least 1",
            "[prices] are keyed by model id, not alias: write [prices.\"claude-haiku-5-5\"] instead of [prices.cheap]",
            "[prices.cheap] input and output must be finite and not negative",
        ]
    );
    assert!(
        error
            .to_string()
            .starts_with("invalid agent config:\n  - [aliases] `blank`")
    );
}

#[test]
fn an_empty_alias_table_is_refused() {
    let error = settings("[aliases]\n").err().unwrap();
    assert_eq!(error.0[0], "[aliases] must define at least one alias");
}

#[test]
fn misspelled_fields_are_refused() {
    let text = format!("{MINIMAL}\n[limits]\nmax_turn = 3\n");
    assert!(toml::from_str::<RawConfig>(&text).is_err());
    let text = format!("{MINIMAL}\n[agent]\nmodle = \"default\"\n");
    assert!(toml::from_str::<RawConfig>(&text).is_err());
}

// ---- 費用 ----

fn step(model: &str, usage: Usage) -> Step {
    Step {
        turn: 1,
        response: Response {
            id: None,
            model: model.into(),
            content: vec![],
            stop: StopReason {
                kind: StopKind::EndTurn,
                raw: "end_turn".into(),
            },
            usage,
            meta: ProviderMetadata::default(),
        },
        latency_ms: 0,
        tool_calls: vec![],
    }
}

#[test]
fn cost_adds_up_every_step_and_counts_cache_as_input() {
    let prices = BTreeMap::from([(
        "m".to_string(),
        Price {
            input: 2.0,
            output: 10.0,
        },
    )]);
    let steps = [
        step(
            "m",
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 100_000,
                ..Usage::default()
            },
        ),
        step(
            "m",
            Usage {
                cache_read_tokens: 500_000,
                cache_write_tokens: 500_000,
                ..Usage::default()
            },
        ),
    ];
    // 第一步:2 + 1 = 3;第二步:cache 1M × 2 = 2。
    assert_eq!(cost(&steps, &prices), Some(5.0));
    assert_eq!(cost(&[], &prices), Some(0.0));
    // 有一步的模型沒有價格:未知,不是只算有價格的部分。
    let mixed = [steps[0].clone(), step("other", Usage::default())];
    assert_eq!(cost(&mixed, &prices), None);
}

// ---- [[mcp]](003 第二部分,design-part2 §6)----

fn problems(text: &str) -> Vec<String> {
    settings(text).err().unwrap().0
}

fn with_mcp(mcp: &str) -> String {
    format!("{MINIMAL}\n{mcp}")
}

#[test]
fn no_mcp_servers_by_default() {
    assert_eq!(settings(MINIMAL).unwrap().mcp, []);
}

#[test]
fn stdio_and_http_servers_are_read() {
    let s = settings(&with_mcp(
        r#"
[[mcp]]
name = "notes"
command = "notes-mcp"
args = ["--root", "/home/me/notes"]
env = { NOTES_DIR = "/home/me/notes" }
tools = ["search", "read"]

[[mcp]]
name = "local"
command = "bin/my-server"
tools = ["x"]

[[mcp]]
name = "remote"
url = "https://example.com/mcp"
token_env = "REMOTE_MCP_SECRET"
tools = ["lookup"]
"#,
    ))
    .unwrap();
    assert_eq!(
        s.mcp,
        [
            McpServer {
                name: "notes".into(),
                transport: McpTransport::Stdio {
                    // 沒有 `/` 的指令照 PATH 找,不改。
                    command: "notes-mcp".into(),
                    args: vec!["--root".into(), "/home/me/notes".into()],
                    env: BTreeMap::from([("NOTES_DIR".into(), "/home/me/notes".into())]),
                },
                tools: vec!["search".into(), "read".into()],
            },
            McpServer {
                name: "local".into(),
                transport: McpTransport::Stdio {
                    // 有 `/` 的相對路徑以設定檔所在的目錄為準(同 [paths])。
                    command: "/conf/bin/my-server".into(),
                    args: vec![],
                    env: BTreeMap::new(),
                },
                tools: vec!["x".into()],
            },
            McpServer {
                name: "remote".into(),
                transport: McpTransport::Http {
                    url: "https://example.com/mcp".into(),
                    token_env: Some("REMOTE_MCP_SECRET".into()),
                },
                tools: vec!["lookup".into()],
            },
        ]
    );
}

#[test]
fn server_names_are_short_lowercase_and_unique() {
    for name in ["", "Notes", "my-notes", "a b", "seventeen_chars_x"] {
        let text = with_mcp(&format!(
            "[[mcp]]\nname = \"{name}\"\ncommand = \"x\"\ntools = [\"t\"]\n"
        ));
        assert_eq!(
            problems(&text),
            [format!(
                "[[mcp]] name `{name}` must be 1-16 lowercase letters, digits or `_`"
            )],
            "{name}"
        );
    }
    let text = with_mcp(
        "[[mcp]]\nname = \"a\"\ncommand = \"x\"\ntools = [\"t\"]\n[[mcp]]\nname = \"a\"\ncommand = \"y\"\ntools = [\"t\"]\n",
    );
    assert_eq!(problems(&text), ["[[mcp]] name `a` is used twice"]);
    // 16 個字元剛好可以。
    let text = with_mcp("[[mcp]]\nname = \"sixteen_chars_xx\"\ncommand = \"x\"\ntools = [\"t\"]\n");
    assert!(settings(&text).is_ok());
}

#[test]
fn exactly_one_transport() {
    let both = with_mcp(
        "[[mcp]]\nname = \"a\"\ncommand = \"x\"\nurl = \"https://e.com/mcp\"\ntools = [\"t\"]\n",
    );
    let neither = with_mcp("[[mcp]]\nname = \"a\"\ntools = [\"t\"]\n");
    for text in [both, neither] {
        assert_eq!(
            problems(&text),
            ["[[mcp]] `a` needs exactly one of `command` (stdio) or `url` (HTTP)"]
        );
    }
}

#[test]
fn transport_options_must_match_the_transport() {
    let text = with_mcp(
        "[[mcp]]\nname = \"a\"\nurl = \"https://e.com/mcp\"\nargs = [\"x\"]\ntools = [\"t\"]\n\
         [[mcp]]\nname = \"b\"\ncommand = \"x\"\ntoken_env = \"T\"\ntools = [\"t\"]\n",
    );
    assert_eq!(
        problems(&text),
        [
            "[[mcp]] `a`: `args` and `env` only apply to `command`",
            "[[mcp]] `b`: `token_env` only applies to `url`",
        ]
    );
}

#[test]
fn urls_tokens_and_env_names_are_checked() {
    let text = with_mcp(
        "[[mcp]]\nname = \"a\"\nurl = \"http://example.com/mcp\"\ntoken_env = \"bad-name\"\ntools = [\"t\"]\n\
         [[mcp]]\nname = \"b\"\ncommand = \"x\"\nenv = { \"1BAD\" = \"v\" }\ntools = [\"t\"]\n\
         [[mcp]]\nname = \"c\"\ncommand = \"\"\ntools = [\"t\"]\n",
    );
    assert_eq!(
        problems(&text),
        [
            "[[mcp]] `a` url `http://example.com/mcp` must use https (plain http is only allowed for localhost)",
            "[[mcp]] `a`: token_env `bad-name` must be an environment variable name (A-Z, a-z, 0-9, `_`, not starting with a digit)",
            "[[mcp]] `b`: env `1BAD` must be an environment variable name (A-Z, a-z, 0-9, `_`, not starting with a digit)",
            "[[mcp]] `c`: `command` must not be empty",
        ]
    );
    // 本機的 http 可以(給本機的 server 與測試)。
    let text =
        with_mcp("[[mcp]]\nname = \"a\"\nurl = \"http://127.0.0.1:3000/mcp\"\ntools = [\"t\"]\n");
    assert!(settings(&text).is_ok());
}

#[test]
fn the_allowlist_is_required_and_every_name_must_work_for_the_model() {
    // 白名單必填(D-050);組出來的 `<server>__<tool>` 要符合 Anthropic 的名稱規則,不然模型呼叫不了。
    let text = with_mcp(
        "[[mcp]]\nname = \"a\"\ncommand = \"x\"\ntools = []\n\
         [[mcp]]\nname = \"b\"\ncommand = \"x\"\ntools = [\"t\", \"t\", \"admin.tools.list\"]\n",
    );
    assert_eq!(
        problems(&text),
        [
            "[[mcp]] `a`: `tools` must list at least one tool (the allowlist)",
            "[[mcp]] `b`: tool `t` is listed twice",
            "[[mcp]] `b`: the tool `admin.tools.list` cannot be offered to the model as `b__admin.tools.list` \
             (names must be 1-64 characters of ASCII letters, digits, `_` or `-`)",
        ]
    );
    // 長度:server 16 + `__` 2 + 工具 46 = 64 剛好可以,47 就不行。
    let ok = with_mcp(&format!(
        "[[mcp]]\nname = \"sixteen_chars_xx\"\ncommand = \"x\"\ntools = [\"{}\"]\n",
        "t".repeat(46)
    ));
    assert!(settings(&ok).is_ok());
    let long = with_mcp(&format!(
        "[[mcp]]\nname = \"sixteen_chars_xx\"\ncommand = \"x\"\ntools = [\"{}\"]\n",
        "t".repeat(47)
    ));
    assert_eq!(problems(&long).len(), 1);
}

#[test]
fn the_example_config_and_its_mcp_examples_are_valid() {
    // README 叫人複製這個檔案:它本身、以及把 [[mcp]] 範例取消註解之後,都要通過檢查。
    let example = include_str!("../../../../agent.example.toml");
    assert!(settings(example).is_ok());
    let keys = [
        "[[mcp]]",
        "name =",
        "command =",
        "args =",
        "env =",
        "tools =",
        "url =",
        "token_env =",
    ];
    let uncommented: String = example
        .lines()
        .map(|line| match line.strip_prefix("# ") {
            Some(rest) if keys.iter().any(|k| rest.starts_with(k)) => rest,
            _ => line,
        })
        .map(|line| format!("{line}\n"))
        .collect();
    let s = settings(&uncommented).unwrap();
    let names: Vec<&str> = s.mcp.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["notes", "remote"]);
}

#[test]
fn misspelled_mcp_fields_are_refused() {
    let text = with_mcp("[[mcp]]\nname = \"a\"\ncommand = \"x\"\ntool = [\"t\"]\n");
    assert!(toml::from_str::<RawConfig>(&text).is_err());
}

// ---- [sandbox](003 第三部分)----

#[test]
fn the_sandbox_defaults_to_the_pinned_image_and_design_limits() {
    let s = settings(MINIMAL).unwrap();
    assert_eq!(s.sandbox, sandbox::ContainerConfig::default());
}

#[test]
fn the_sandbox_can_be_tuned_and_a_relative_program_is_resolved() {
    let s = settings(&format!(
        "{MINIMAL}[sandbox]\nprogram = \"bin/docker\"\nrun_timeout_secs = 2\nmemory_mib = 256\n"
    ))
    .unwrap();
    assert_eq!(s.sandbox.program, "/conf/bin/docker");
    assert_eq!(
        s.sandbox.limits.run_timeout,
        std::time::Duration::from_secs(2)
    );
    assert_eq!(s.sandbox.limits.memory_bytes, 256 * 1024 * 1024);
    // 沒有 `/`:照 PATH 找,原樣保留。
    let s = settings(&format!("{MINIMAL}[sandbox]\nprogram = \"podman\"\n")).unwrap();
    assert_eq!(s.sandbox.program, "podman");
}

#[test]
fn sandbox_problems_are_listed_with_the_others() {
    assert_eq!(
        problems(&format!("{MINIMAL}[sandbox]\npids = 0\n")),
        ["invalid sandbox configuration: the sandbox limit `pids` must be greater than 0"]
    );
    assert!(toml::from_str::<RawConfig>(&format!("{MINIMAL}[sandbox]\nnetwork = true\n")).is_err());
}

#[test]
fn run_rust_needs_a_tool_timeout_longer_than_the_sandbox() {
    // 載入時不檢查(不用 run_rust 的設定不受影響),`--allow-run` 時才檢查。
    let short = settings(&format!("{MINIMAL}[limits]\ntool_timeout_secs = 25\n")).unwrap();
    assert_eq!(
        short.check_run_timeout(),
        Err(
            "limits.tool_timeout_secs (25) must be larger than sandbox.compile_timeout_secs + \
             sandbox.run_timeout_secs (25), or run_rust is cut off before the sandbox finishes"
                .to_string()
        )
    );
    let enough = settings(&format!("{MINIMAL}[limits]\ntool_timeout_secs = 26\n")).unwrap();
    assert_eq!(enough.check_run_timeout(), Ok(()));
    assert_eq!(settings(MINIMAL).unwrap().check_run_timeout(), Ok(()));
}
