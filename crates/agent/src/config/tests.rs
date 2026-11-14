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
