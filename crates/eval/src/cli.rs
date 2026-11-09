//! 命令列(design §8)。
//!
//! ```text
//! eval run --dataset rust-basics --model cheap [--repeats 3] [--tags rust,web] [--limit 5]
//!          [--label "新 system prompt"] [--concurrency 4] [--budget 2.00] [--dry-run]
//! eval run --resume <執行目錄> [--retry-errors] [--budget 5.00] [--dry-run]
//! eval score <執行目錄> [--judge default | --no-judge] [--name sonnet-judge] [--judge-all] [--budget 1.00] [--dry-run]
//! eval compare <執行目錄 A> <執行目錄 B> [--scores sonnet-judge] [--allow-mismatch]
//! ```
//!
//! 用 `clap` 的 derive:struct 的欄位就是參數,`#[arg(...)]` 寫規則(必填、互斥、範圍)。
//! 型別在編譯期就檢查好,解析完拿到的就是 `RunArgs` 這種具體型別。Java 對照:picocli 的 `@Option` 註解,
//! 但這裡規則違反時由 clap 印出用法並結束,程式本身拿不到不合法的組合。

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// 設定檔的預設位置(目前目錄)。
pub const DEFAULT_CONFIG: &str = "eval.toml";
/// 只做確定性檢查時,打分的預設名稱。
pub const CHECKS_ONLY: &str = "checks";

#[derive(Debug, Parser)]
#[command(name = "eval", version, about = "Run, score and compare LLM evals")]
pub struct Cli {
    // `///` 的 doc comment 會變成 `--help` 的說明文字,所以參數的說明用英文寫(跟其他 CLI 輸出一致)。
    /// Config file (relative to the current directory).
    #[arg(long, global = true, default_value = DEFAULT_CONFIG)]
    pub config: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

// `Subcommand` 的 enum:每個 variant 是一個子命令。Java 對照:picocli 的 `subcommands = {...}`。
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Call the model on every case of a dataset and record the answers.
    Run(RunArgs),
    /// Score the answers of a run (deterministic checks, then the LLM judge).
    Score(ScoreArgs),
    /// Compare the scores of two runs.
    Compare(CompareArgs),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    // 新的執行要有資料集與模型;接續時這些設定一律從 `meta.json` 讀回來(design §5),
    // 所以跟 `--resume` 互斥:同一次執行不能混兩種設定。
    /// Dataset name (private first, then public) or path to a .jsonl file.
    #[arg(long, required_unless_present = "resume", conflicts_with = "resume")]
    pub dataset: Option<String>,
    /// Model alias from [aliases].
    #[arg(long, required_unless_present = "resume", conflicts_with = "resume")]
    pub model: Option<String>,
    /// How many times to ask each case.
    // `value_parser!(u32).range(..)`:範圍在解析時就檢查,超出時 clap 印出錯誤與用法。
    // 預設值不算「使用者給了」,所以 `--resume` 搭配預設的 1 不會被當成衝突。
    #[arg(long, default_value_t = 1, conflicts_with = "resume",
          value_parser = clap::value_parser!(u32).range(1..=100))]
    pub repeats: u32,
    /// Only cases with any of these tags (comma-separated).
    // `value_delimiter = ','`:`--tags rust,web` 拆成兩個值。
    #[arg(long, value_delimiter = ',', conflicts_with = "resume")]
    pub tags: Vec<String>,
    /// Only the first N cases after the tag filter.
    #[arg(long, conflicts_with = "resume", value_parser = at_least_one)]
    pub limit: Option<usize>,
    /// A note for humans, e.g. "new system prompt".
    #[arg(long, conflicts_with = "resume")]
    pub label: Option<String>,
    /// Calls in flight at once (overrides [run] concurrency).
    #[arg(long, value_parser = concurrency)]
    pub concurrency: Option<usize>,
    /// Stop sending new calls once the run has cost this many US dollars.
    #[arg(long, value_parser = positive_usd)]
    pub budget: Option<f64>,
    /// Print what would run and the worst-case cost, without calling the model.
    #[arg(long)]
    pub dry_run: bool,
    /// Continue an interrupted run instead of starting a new one.
    #[arg(long, value_name = "RUN_DIR")]
    pub resume: Option<PathBuf>,
    /// With --resume: also redo calls that failed.
    // 只寫 `requires = "resume"` 不夠:被要求的 `resume` 跟已經出現的 `--dataset` 互斥時,clap 不會檢查 requires。
    // 沒有 `--resume` 時 `--dataset` 一定在(必填),所以跟它互斥就等於「只能搭配 `--resume`」。
    #[arg(long, requires = "resume", conflicts_with = "dataset")]
    pub retry_errors: bool,
}

#[derive(Debug, Args)]
pub struct ScoreArgs {
    /// The run directory.
    pub run: PathBuf,
    /// Judge alias (default: [judge] alias).
    #[arg(long, conflicts_with = "no_judge")]
    pub judge: Option<String>,
    /// Deterministic checks only; refused if a case has a rubric.
    #[arg(long)]
    pub no_judge: bool,
    /// Name of this scoring, scores/<name>.jsonl (default: the judge alias, or "checks").
    #[arg(long)]
    pub name: Option<String>,
    /// Call the judge even when a check already failed.
    #[arg(long, conflicts_with = "no_judge")]
    pub judge_all: bool,
    /// Judge calls in flight at once (overrides [run] concurrency).
    #[arg(long, value_parser = concurrency)]
    pub concurrency: Option<usize>,
    /// Stop calling the judge once scoring has cost this many US dollars.
    #[arg(long, value_parser = positive_usd)]
    pub budget: Option<f64>,
    /// Print how many judge calls are needed and the worst-case cost, without calling the judge.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct CompareArgs {
    /// Run directory A (the baseline).
    pub run_a: PathBuf,
    /// Run directory B (the change).
    pub run_b: PathBuf,
    /// Which scoring to compare (default: the only one both runs have).
    #[arg(long)]
    pub scores: Option<String>,
    /// Compare even if the two scorings used different settings (the report says so at the top).
    #[arg(long)]
    pub allow_mismatch: bool,
}

impl ScoreArgs {
    /// 這次要用的評審別名;`--no-judge` 時是 `None`。
    // `'a`:回傳的字串借自 `self` 或 `configured`,兩者都要活得比回傳值久。
    pub fn judge_alias<'a>(&'a self, configured: &'a str) -> Option<&'a str> {
        if self.no_judge {
            None
        } else {
            Some(self.judge.as_deref().unwrap_or(configured))
        }
    }

    /// 打分的名稱:`--name`,否則是評審別名,只做檢查時是 `checks`。
    pub fn score_name(&self, configured_judge: &str) -> String {
        match (&self.name, self.judge_alias(configured_judge)) {
            (Some(name), _) => name.clone(),
            (None, Some(judge)) => judge.to_owned(),
            (None, None) => CHECKS_ONLY.to_owned(),
        }
    }
}

impl ScoreArgs {
    /// 確定性檢查沒過時要不要跳過評審:設定檔說要跳過,而且這次沒有 `--judge-all`(D-034)。
    pub fn skip_on_check_failure(&self, configured: bool) -> bool {
        configured && !self.judge_all
    }
}

/// `--budget` 的值:正的、有限的美元數。0 或負數沒有意義(什麼都不會做),NaN 會讓比較永遠不成立。
pub fn positive_usd(raw: &str) -> Result<f64, String> {
    match raw.parse::<f64>() {
        Ok(usd) if usd.is_finite() && usd > 0.0 => Ok(usd),
        _ => Err("must be a positive number of US dollars, like 2.00".into()),
    }
}

/// `--concurrency`:跟設定檔的 `[run] concurrency` 同一個範圍。
fn concurrency(raw: &str) -> Result<usize, String> {
    match raw.parse::<usize>() {
        Ok(n) if (1..=crate::config::MAX_CONCURRENCY).contains(&n) => Ok(n),
        _ => Err(format!(
            "must be between 1 and {}",
            crate::config::MAX_CONCURRENCY
        )),
    }
}

/// `--limit 0` 什麼都不會跑,一定是打錯了。
fn at_least_one(raw: &str) -> Result<usize, String> {
    match raw.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err("must be at least 1".into()),
    }
}

#[cfg(test)]
mod tests;
