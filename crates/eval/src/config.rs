//! `eval.toml` 的形狀與檢查(design §8),以及 `--dataset` 名稱怎麼找到檔案(design §4)。
//!
//! 跟 gateway 一樣分兩層:`RawConfig` 是檔案裡寫什麼就是什麼;`Settings` 是檢查過、補上預設值、
//! 路徑解析好的結果,之後的程式只看得到這一層。讀檔與讀環境變數只在 `main.rs`(D-016),這裡全是純函式。

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use config_loader::ConfigFile;
use llm::{DEFAULT_BASE_URL, RetryPolicy};
use serde::Deserialize;

use crate::run::Price;

pub const DEFAULT_CONCURRENCY: usize = 4;
/// 並行上限的上限:設定寫錯(多打一個 0)時,不能一口氣送出幾百個呼叫、燒掉 API 額度。
pub const MAX_CONCURRENCY: usize = 64;
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
pub const DEFAULT_JUDGE_EFFORT: &str = "low";
/// 公開資料集的預設目錄(相對於設定檔所在的目錄)。
pub const DEFAULT_PUBLIC_DATASETS: &str = "evals";

/// 設定檔的原始內容。每個區段都 `deny_unknown_fields`:打錯的欄位名稱要報錯,不能被默默忽略。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    pub aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub anthropic: AnthropicSection,
    pub judge: JudgeSection,
    #[serde(default)]
    pub run: RunSection,
    #[serde(default)]
    pub paths: PathsSection,
    #[serde(default)]
    pub prices: BTreeMap<String, Price>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AnthropicSection {
    pub base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeSection {
    /// 必填:評審是哪個模型,要明確寫出來(它決定分數的意思)。
    pub alias: String,
    #[serde(default = "default_effort")]
    pub effort: String,
    #[serde(default = "default_true")]
    pub skip_on_check_failure: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RunSection {
    pub concurrency: usize,
    pub timeout_secs: u64,
}

impl Default for RunSection {
    fn default() -> Self {
        Self {
            concurrency: DEFAULT_CONCURRENCY,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PathsSection {
    pub data_dir: Option<String>,
    pub datasets: String,
    /// gateway 的呼叫紀錄(`eval add` 從這裡挑案例)。預設跟 gateway 一樣。
    pub records: Option<String>,
}

impl Default for PathsSection {
    fn default() -> Self {
        Self {
            data_dir: None,
            datasets: DEFAULT_PUBLIC_DATASETS.into(),
            records: None,
        }
    }
}

fn default_effort() -> String {
    DEFAULT_JUDGE_EFFORT.into()
}

fn default_true() -> bool {
    true
}

/// 檢查過的設定。
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub aliases: BTreeMap<String, String>,
    pub base_url: String,
    pub judge: JudgeSettings,
    pub concurrency: usize,
    pub timeout: Duration,
    /// key 是實際的模型 id。
    pub prices: BTreeMap<String, Price>,
    /// 每次執行一個目錄(`<data_dir>/runs`)。
    pub runs_dir: PathBuf,
    /// 私有資料集(`<data_dir>/datasets`)。
    pub private_datasets: PathBuf,
    pub public_datasets: PathBuf,
    /// gateway 的呼叫紀錄目錄(一天一個 JSONL 檔)。
    pub records_dir: PathBuf,
    pub retry: RetryPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeSettings {
    pub alias: String,
    pub effort: String,
    pub skip_on_check_failure: bool,
}

/// 設定有問題。一次列出全部的問題。
#[derive(Debug, thiserror::Error)]
#[error("invalid eval config:\n{}", .0.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n"))]
pub struct InvalidConfig(pub Vec<String>);

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown model alias `{alias}`; [aliases] in the config has: {}", known.join(", "))]
pub struct UnknownAlias {
    pub alias: String,
    pub known: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DatasetLookupError {
    #[error(
        "invalid dataset name {0:?}: use letters, digits, `.`, `_` or `-`, or give a path to a .jsonl file"
    )]
    InvalidName(String),

    #[error("dataset `{name}` not found; looked for {}", looked.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(" and "))]
    NotFound { name: String, looked: Vec<PathBuf> },

    #[error(
        "dataset `{name}` exists both as a private dataset ({}) and a public one ({}); rename one, or give the path",
        private.display(),
        public.display()
    )]
    Ambiguous {
        name: String,
        private: PathBuf,
        public: PathBuf,
    },

    #[error("{} is accessible by other users (mode {mode:o}); run `chmod 700` on it", dir.display())]
    NotPrivate { dir: PathBuf, mode: u32 },

    #[error(
        "a public dataset named `{name}` exists ({}); `eval add` only writes private datasets, so pick another name",
        public.display()
    )]
    PublicNameTaken { name: String, public: PathBuf },
}

impl Settings {
    /// 檢查設定、補上預設值、解析路徑。
    ///
    /// `default_data_dir`:某個程式(`eval`、`gateway`)的預設資料目錄,沒寫路徑時用。
    /// 用函式傳進來,測試才不必碰真的家目錄。正式版是 `config_loader::default_data_dir`。
    pub fn from_config(
        file: &ConfigFile<RawConfig>,
        default_data_dir: impl Fn(&str) -> config_loader::Result<PathBuf>,
    ) -> Result<Self, InvalidConfig> {
        let raw = &file.value;
        // 每個檢查都把問題丟進同一個清單,不提早 return:最後一次全部列出來(同 gateway)。
        let mut problems = Vec::new();

        if raw.aliases.is_empty() {
            problems.push("[aliases] must define at least one alias".into());
        }
        for (alias, model) in &raw.aliases {
            if alias.trim().is_empty() || model.trim().is_empty() {
                problems.push(format!(
                    "[aliases] `{alias}` must map a non-empty name to a non-empty model id"
                ));
            }
        }
        let base_url = config_loader::check_api_base_url(
            "anthropic.base_url",
            raw.anthropic
                .base_url
                .as_deref()
                .unwrap_or(DEFAULT_BASE_URL),
        )
        .map_err(|problem| problems.push(problem))
        .ok();
        if !raw.aliases.contains_key(&raw.judge.alias) {
            problems.push(format!(
                "judge.alias `{}` is not defined in [aliases]",
                raw.judge.alias
            ));
        }
        // effort 的合法值由 API 決定(而且會增加),這裡只擋空的;寫錯的值第一次呼叫評審時會得到 400。
        if raw.judge.effort.trim().is_empty() {
            problems.push("judge.effort must not be empty".into());
        }
        if !(1..=MAX_CONCURRENCY).contains(&raw.run.concurrency) {
            problems.push(format!(
                "run.concurrency must be between 1 and {MAX_CONCURRENCY}, got {}",
                raw.run.concurrency
            ));
        }
        if raw.run.timeout_secs == 0 {
            problems.push("run.timeout_secs must be at least 1".into());
        }
        for (model, price) in &raw.prices {
            // 最容易寫錯的地方:用別名當 key。那樣價格永遠查不到,`--budget` 會拒絕執行,原因卻不明顯。
            if let Some(id) = raw.aliases.get(model) {
                problems.push(format!(
                    "[prices] are keyed by model id, not alias: write [prices.\"{id}\"] instead of [prices.{model}]"
                ));
            }
            // `is_finite`:不是 NaN、也不是無限大。NaN 跟任何數比較都是 false,預算就永遠不會「超過」。
            let valid = |v: f64| v.is_finite() && v >= 0.0;
            if !valid(price.input) || !valid(price.output) {
                problems.push(format!(
                    "[prices.{model}] input and output must be finite and not negative"
                ));
            }
        }

        let data_dir = match &raw.paths.data_dir {
            // 相對路徑以設定檔所在的目錄為準(config-loader,D-025)。
            Some(dir) => file
                .resolve(dir)
                .map_err(|e| problems.push(format!("paths.data_dir: {e}")))
                .ok(),
            None => default_data_dir("eval")
                .map_err(|e| problems.push(format!("default data directory: {e}")))
                .ok(),
        };
        let records_dir = match &raw.paths.records {
            Some(dir) => file
                .resolve(dir)
                .map_err(|e| problems.push(format!("paths.records: {e}")))
                .ok(),
            // 跟 gateway 的預設一樣(gateway 的 `Settings::from_config`),才找得到 gateway 寫的紀錄。
            None => default_data_dir("gateway")
                .map(|dir| dir.join("records"))
                .map_err(|e| problems.push(format!("default records directory: {e}")))
                .ok(),
        };
        let public_datasets = file
            .resolve(&raw.paths.datasets)
            .map_err(|e| problems.push(format!("paths.datasets: {e}")))
            .ok();

        // 只有「沒有任何問題」時,上面那些 Option 才一定都是 Some;寫在同一個 match 裡就不需要 unwrap(同 gateway)。
        match (base_url, data_dir, public_datasets, records_dir) {
            (Some(base_url), Some(data_dir), Some(public_datasets), Some(records_dir))
                if problems.is_empty() =>
            {
                Ok(Self {
                    aliases: raw.aliases.clone(),
                    base_url,
                    judge: JudgeSettings {
                        alias: raw.judge.alias.clone(),
                        effort: raw.judge.effort.clone(),
                        skip_on_check_failure: raw.judge.skip_on_check_failure,
                    },
                    concurrency: raw.run.concurrency,
                    timeout: Duration::from_secs(raw.run.timeout_secs),
                    prices: raw.prices.clone(),
                    runs_dir: data_dir.join("runs"),
                    private_datasets: data_dir.join("datasets"),
                    public_datasets,
                    records_dir,
                    retry: RetryPolicy::default(),
                })
            }
            _ => Err(InvalidConfig(problems)),
        }
    }

    /// 別名不在 `[aliases]` 裡:先擋下。否則每一次呼叫都會被 Alias 拒絕,整次執行全是錯誤,還留下一個執行目錄。
    pub fn require_alias(&self, alias: &str) -> Result<(), UnknownAlias> {
        if self.aliases.contains_key(alias) {
            Ok(())
        } else {
            Err(UnknownAlias {
                alias: alias.to_owned(),
                known: self.aliases.keys().cloned().collect(),
            })
        }
    }

    /// 別名(或已經是實際 id)對應的價格。價格的 key 是實際的模型 id。
    pub fn price_of(&self, model: &str) -> Option<Price> {
        let id = self.aliases.get(model).map_or(model, String::as_str);
        self.prices.get(id).copied()
    }

    /// `--dataset` 的值 → 資料集檔案。
    ///
    /// - 看起來像路徑(含 `/` 或以 `.jsonl` 結尾):直接用,轉成絕對路徑(執行的 `meta.json` 會記下它,接續時要找得到)。
    /// - 否則是名稱:先找私有的 `<data_dir>/datasets/<名稱>.jsonl`,再找公開的 `<datasets>/<名稱>.jsonl`;
    ///   兩邊都有就拒絕(避免搞混,design §4)。私有資料集的目錄別人讀得到也拒絕(design §9)。
    pub fn find_dataset(&self, arg: &str) -> Result<PathBuf, DatasetLookupError> {
        if arg.contains('/') || arg.ends_with(".jsonl") {
            // `std::path::absolute`:接上目前的工作目錄,不碰檔案系統(檔案不存在時,由讀資料集的地方報錯)。
            return std::path::absolute(arg)
                .map_err(|_| DatasetLookupError::InvalidName(arg.to_owned()));
        }
        if !valid_name(arg) {
            return Err(DatasetLookupError::InvalidName(arg.to_owned()));
        }
        let file = format!("{arg}.jsonl");
        let private = self.private_datasets.join(&file);
        let public = self.public_datasets.join(&file);
        match (private.is_file(), public.is_file()) {
            (true, true) => Err(DatasetLookupError::Ambiguous {
                name: arg.to_owned(),
                private,
                public,
            }),
            (true, false) => {
                check_private(&self.private_datasets)?;
                Ok(private)
            }
            (false, true) => Ok(public),
            (false, false) => Err(DatasetLookupError::NotFound {
                name: arg.to_owned(),
                looked: vec![private, public],
            }),
        }
    }
}

impl Settings {
    /// `eval add` 要寫入的私有資料集檔案。只接受名稱(不接受路徑:不能寫進 repo 裡的 `evals/`,D-030);
    /// 公開資料集已經有同名的也拒絕(之後 `--dataset` 會分不清是哪一個,design §4)。
    pub fn private_dataset_path(&self, name: &str) -> Result<PathBuf, DatasetLookupError> {
        if !valid_name(name) {
            return Err(DatasetLookupError::InvalidName(name.to_owned()));
        }
        let file = format!("{name}.jsonl");
        let public = self.public_datasets.join(&file);
        if public.exists() {
            return Err(DatasetLookupError::PublicNameTaken {
                name: name.to_owned(),
                public,
            });
        }
        Ok(self.private_datasets.join(file))
    }
}

/// 資料集名稱:英數字開頭,只用英數字與 `.`、`_`、`-`。不能有 `/`、不能是 `..`:找檔案時不能跑出資料集目錄。
fn valid_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// 私有資料集是真實對話:目錄只能自己進得去(同 D-021 的規則)。
fn check_private(dir: &Path) -> Result<(), DatasetLookupError> {
    // 讀不到權限就當成不安全:讀取路徑 fail-closed。
    let mode = std::fs::metadata(dir).map_or(0o777, |m| m.permissions().mode() & 0o777);
    if mode & 0o077 != 0 {
        return Err(DatasetLookupError::NotPrivate {
            dir: dir.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
