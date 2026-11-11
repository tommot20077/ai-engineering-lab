use std::fs::Permissions;

use super::*;

/// 把 TOML 寫成暫存目錄裡的 `eval.toml` 再讀回來(相對路徑以它所在的目錄為準)。
fn load(text: &str) -> (tempfile::TempDir, ConfigFile<RawConfig>) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("eval.toml");
    std::fs::write(&path, text).unwrap();
    let file = config_loader::load_toml(&path).unwrap();
    (tmp, file)
}

fn settings(text: &str) -> Result<Settings, InvalidConfig> {
    let (_tmp, file) = load(text);
    Settings::from_config(&file, fake_data_dir)
}

/// 假的預設資料目錄:`/data/<程式名稱>`。
fn fake_data_dir(app: &str) -> config_loader::Result<PathBuf> {
    Ok(PathBuf::from(format!("/data/{app}")))
}

/// 必填的最小設定。
const MINIMAL: &str = "[aliases]\ncheap = \"claude-haiku-5-5\"\n[judge]\nalias = \"cheap\"\n";

fn problems(text: &str) -> Vec<String> {
    match settings(text) {
        Ok(s) => panic!("expected problems, got {s:?}"),
        Err(InvalidConfig(problems)) => problems,
    }
}

#[test]
fn the_example_file_is_a_valid_config() {
    // 範例檔是給人複製的:它本身一定要能用(改了格式卻忘了改範例,這個測試會失敗)。
    let s = settings(include_str!("../../../../eval.example.toml")).unwrap();
    assert_eq!(
        s.aliases.get("cheap").map(String::as_str),
        Some("claude-haiku-5-5")
    );
    assert_eq!(s.judge.alias, "default");
    // 範例刻意不填價格:沒有價格就不能用 --budget(fail-closed)。
    assert!(s.prices.is_empty());
}

#[test]
fn defaults_fill_in_everything_that_is_optional() {
    let (tmp, file) = load(MINIMAL);
    let s = Settings::from_config(&file, fake_data_dir).unwrap();

    assert_eq!(
        s,
        Settings {
            aliases: [("cheap".to_owned(), "claude-haiku-5-5".to_owned())].into(),
            base_url: DEFAULT_BASE_URL.into(),
            judge: JudgeSettings {
                alias: "cheap".into(),
                effort: "low".into(),
                skip_on_check_failure: true,
            },
            concurrency: 4,
            timeout: Duration::from_secs(300),
            prices: BTreeMap::new(),
            runs_dir: "/data/eval/runs".into(),
            calibrations_dir: "/data/eval/calibrations".into(),
            private_datasets: "/data/eval/datasets".into(),
            // 相對路徑以設定檔所在的目錄為準(config-loader,D-025)。
            public_datasets: tmp.path().join("evals"),
            // 跟 gateway 的預設一樣:gateway 資料目錄底下的 records。
            records_dir: "/data/gateway/records".into(),
            retry: RetryPolicy::default(),
        }
    );
}

#[test]
fn every_section_can_be_set() {
    let s = settings(
        r#"
[aliases]
default = "claude-sonnet-5-5"
cheap = "claude-haiku-5-5"
[anthropic]
base_url = "http://127.0.0.1:9000"
[judge]
alias = "default"
effort = "medium"
skip_on_check_failure = false
[run]
concurrency = 8
timeout_secs = 60
[paths]
data_dir = "/srv/eval"
datasets = "/repo/evals"
records = "gw/records"
[prices."claude-haiku-5-5"]
input = 1.0
output = 5.0
"#,
    )
    .unwrap();

    assert_eq!(s.base_url, "http://127.0.0.1:9000");
    assert_eq!(
        s.judge,
        JudgeSettings {
            alias: "default".into(),
            effort: "medium".into(),
            skip_on_check_failure: false
        }
    );
    assert_eq!((s.concurrency, s.timeout), (8, Duration::from_secs(60)));
    assert_eq!(s.runs_dir, Path::new("/srv/eval/runs"));
    assert_eq!(s.private_datasets, Path::new("/srv/eval/datasets"));
    assert_eq!(s.public_datasets, Path::new("/repo/evals"));
    // 相對路徑以設定檔所在的目錄為準。
    assert!(
        s.records_dir.ends_with("gw/records") && s.records_dir.is_absolute(),
        "{:?}",
        s.records_dir
    );
    // 價格依實際的模型 id 存;用別名或 id 都查得到。
    let haiku = Some(Price {
        input: 1.0,
        output: 5.0,
    });
    assert_eq!(s.price_of("cheap"), haiku);
    assert_eq!(s.price_of("claude-haiku-5-5"), haiku);
    assert_eq!(s.price_of("default"), None);
    assert_eq!(s.price_of("unknown"), None);
}

#[test]
fn every_problem_is_reported_at_once() {
    let got = problems(
        r#"
[aliases]
cheap = ""
default = "claude-sonnet-5-5"
[anthropic]
base_url = "http://api.example.com"
[judge]
alias = "nope"
effort = " "
[run]
concurrency = 0
timeout_secs = 0
[prices.default]
input = -1.0
output = 1.0
"#,
    );

    assert_eq!(
        got,
        [
            "[aliases] `cheap` must map a non-empty name to a non-empty model id",
            "anthropic.base_url `http://api.example.com` must use https (plain http is only allowed for localhost)",
            "judge.alias `nope` is not defined in [aliases]",
            "judge.effort must not be empty",
            "run.concurrency must be between 1 and 64, got 0",
            "run.timeout_secs must be at least 1",
            "[prices] are keyed by model id, not alias: write [prices.\"claude-sonnet-5-5\"] instead of [prices.default]",
            "[prices.default] input and output must be finite and not negative",
        ]
    );
}

#[test]
fn aliases_must_not_be_empty_and_concurrency_has_a_ceiling() {
    let got = problems("[aliases]\n[judge]\nalias = \"x\"\n[run]\nconcurrency = 65\n");
    assert_eq!(
        got,
        [
            "[aliases] must define at least one alias",
            "judge.alias `x` is not defined in [aliases]",
            "run.concurrency must be between 1 and 64, got 65",
        ]
    );
    // 上限本身可以。
    let at_limit = format!("{MINIMAL}[run]\nconcurrency = 64\n");
    assert_eq!(settings(&at_limit).unwrap().concurrency, 64);
}

#[test]
fn prices_keyed_by_an_alias_name_point_to_the_model_id() {
    let got = problems(&format!(
        "{MINIMAL}[prices.cheap]\ninput = 1.0\noutput = 2.0\n"
    ));
    assert_eq!(
        got,
        [
            "[prices] are keyed by model id, not alias: write [prices.\"claude-haiku-5-5\"] instead of [prices.cheap]"
        ]
    );
    // 價格 0 是合法的(例如免費的模型);NaN、無限大不是。
    let free = format!("{MINIMAL}[prices.\"claude-haiku-5-5\"]\ninput = 0.0\noutput = 0.0\n");
    assert!(settings(&free).is_ok());
    let nan = format!("{MINIMAL}[prices.\"claude-haiku-5-5\"]\ninput = nan\noutput = 1.0\n");
    assert_eq!(
        problems(&nan),
        ["[prices.claude-haiku-5-5] input and output must be finite and not negative"]
    );
    let inf = format!("{MINIMAL}[prices.\"claude-haiku-5-5\"]\ninput = 1.0\noutput = inf\n");
    assert_eq!(problems(&inf).len(), 1);
}

#[test]
fn a_missing_default_data_dir_is_a_problem_not_a_panic() {
    let no_home = |_: &str| Err(config_loader::ConfigError::NoHome);
    let (_tmp, file) = load(MINIMAL);
    let err = Settings::from_config(&file, no_home).unwrap_err();
    assert_eq!(
        err.0,
        [
            "default data directory: cannot find the home directory: HOME is not set",
            "default records directory: cannot find the home directory: HOME is not set",
        ]
    );
    // 兩個路徑都寫了,就不需要預設值。
    let explicit =
        format!("{MINIMAL}[paths]\ndata_dir = \"/srv/eval\"\nrecords = \"/srv/records\"\n");
    let (_tmp, file) = load(&explicit);
    assert!(Settings::from_config(&file, no_home).is_ok());
}

#[test]
fn unknown_fields_are_rejected_when_reading_the_file() {
    // 欄位打錯(`ouput`、`concurency`)在讀檔時就失敗,不會被默默忽略。
    for text in [
        format!("{MINIMAL}[prices.\"claude-haiku-5-5\"]\ninput = 1.0\nouput = 2.0\n"),
        format!("{MINIMAL}[run]\nconcurency = 2\n"),
        format!("{MINIMAL}[judge2]\nalias = \"cheap\"\n"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("eval.toml");
        std::fs::write(&path, &text).unwrap();
        let err = config_loader::load_toml::<RawConfig>(&path).unwrap_err();
        assert!(
            matches!(err, config_loader::ConfigError::Parse { .. }),
            "{text}: {err}"
        );
    }
}

// ---- 找資料集 ----

/// 有私有與公開兩個資料集目錄的設定。私有目錄 700。
fn with_datasets() -> (tempfile::TempDir, Settings) {
    let tmp = tempfile::tempdir().unwrap();
    let private = tmp.path().join("data/datasets");
    let public = tmp.path().join("repo/evals");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::create_dir_all(&public).unwrap();
    std::fs::set_permissions(&private, Permissions::from_mode(0o700)).unwrap();
    let mut s = settings(MINIMAL).unwrap();
    s.private_datasets = private;
    s.public_datasets = public;
    (tmp, s)
}

#[test]
fn a_name_is_looked_up_in_the_private_then_the_public_directory() {
    let (_tmp, s) = with_datasets();
    std::fs::write(s.private_datasets.join("personal.jsonl"), "").unwrap();
    std::fs::write(s.public_datasets.join("rust-basics.jsonl"), "").unwrap();

    assert_eq!(
        s.find_dataset("personal").unwrap(),
        s.private_datasets.join("personal.jsonl")
    );
    assert_eq!(
        s.find_dataset("rust-basics").unwrap(),
        s.public_datasets.join("rust-basics.jsonl")
    );
    assert_eq!(
        s.find_dataset("nope").unwrap_err(),
        DatasetLookupError::NotFound {
            name: "nope".into(),
            looked: vec![
                s.private_datasets.join("nope.jsonl"),
                s.public_datasets.join("nope.jsonl")
            ],
        }
    );
}

#[test]
fn the_same_name_in_both_directories_is_refused() {
    let (_tmp, s) = with_datasets();
    std::fs::write(s.private_datasets.join("smoke.jsonl"), "").unwrap();
    std::fs::write(s.public_datasets.join("smoke.jsonl"), "").unwrap();

    assert_eq!(
        s.find_dataset("smoke").unwrap_err(),
        DatasetLookupError::Ambiguous {
            name: "smoke".into(),
            private: s.private_datasets.join("smoke.jsonl"),
            public: s.public_datasets.join("smoke.jsonl"),
        }
    );
}

#[test]
fn a_private_dataset_in_a_directory_others_can_read_is_refused() {
    let (_tmp, s) = with_datasets();
    std::fs::write(s.private_datasets.join("personal.jsonl"), "").unwrap();
    std::fs::set_permissions(&s.private_datasets, Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        s.find_dataset("personal").unwrap_err(),
        DatasetLookupError::NotPrivate {
            dir: s.private_datasets.clone(),
            mode: 0o755
        }
    );
    // 公開資料集不受影響(本來就在 repo 裡)。
    std::fs::write(s.public_datasets.join("rust-basics.jsonl"), "").unwrap();
    assert!(s.find_dataset("rust-basics").is_ok());
}

#[test]
fn a_path_is_used_as_is_but_made_absolute() {
    let (_tmp, s) = with_datasets();
    assert_eq!(
        s.find_dataset("/somewhere/ds.jsonl").unwrap(),
        Path::new("/somewhere/ds.jsonl")
    );
    // 相對路徑接上目前的工作目錄:`meta.json` 記下的路徑,換個目錄接續時也找得到。
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        s.find_dataset("evals/x.jsonl").unwrap(),
        cwd.join("evals/x.jsonl")
    );
    assert_eq!(s.find_dataset("x.jsonl").unwrap(), cwd.join("x.jsonl"));
}

#[test]
fn names_that_could_escape_the_directories_are_refused() {
    let (_tmp, s) = with_datasets();
    for bad in ["", "..", ".hidden", "-flag", "a b", "有中文"] {
        assert_eq!(
            s.find_dataset(bad).unwrap_err(),
            DatasetLookupError::InvalidName(bad.into()),
            "{bad:?}"
        );
    }
}

#[test]
fn only_aliases_from_the_config_are_accepted() {
    // (cargo-mutants:這段原本在 main.rs,binary 沒有單元測試,換成 `Ok(())` 沒人發現。)
    let s = settings(
        "[aliases]\ncheap = \"claude-haiku-5-5\"\ndefault = \"claude-sonnet-5-5\"\n[judge]\nalias = \"cheap\"\n",
    )
    .unwrap();
    assert_eq!(s.require_alias("cheap"), Ok(()));
    let err = s.require_alias("claude-haiku-5-5").unwrap_err();
    assert_eq!(
        err.to_string(),
        "unknown model alias `claude-haiku-5-5`; [aliases] in the config has: cheap, default"
    );
}

#[test]
fn eval_add_writes_only_private_datasets_by_name() {
    let (_tmp, s) = with_datasets();
    assert_eq!(
        s.private_dataset_path("personal").unwrap(),
        s.private_datasets.join("personal.jsonl")
    );
    // 不收路徑:`evals/x.jsonl` 會寫進 repo(公開),D-030。
    for bad in ["evals/x.jsonl", "/tmp/x.jsonl", "..", ""] {
        assert_eq!(
            s.private_dataset_path(bad).unwrap_err(),
            DatasetLookupError::InvalidName(bad.into()),
            "{bad:?}"
        );
    }
    // 公開資料集已經有同名的:之後 `--dataset` 會分不清,先擋下。
    std::fs::write(s.public_datasets.join("smoke.jsonl"), "").unwrap();
    assert_eq!(
        s.private_dataset_path("smoke").unwrap_err(),
        DatasetLookupError::PublicNameTaken {
            name: "smoke".into(),
            public: s.public_datasets.join("smoke.jsonl"),
        }
    );
}
