//! eval 的 composition root:解析命令列 → 讀設定 → 組裝 provider → 呼叫 library → 印出結果。
//!
//! ```text
//! cp eval.example.toml eval.toml                   # 改成你要的設定
//! cargo run -p eval -- run --dataset smoke --model cheap --dry-run
//! ```
//!
//! 整個 eval 只有這個檔案讀設定檔、環境變數與 `.env`(D-016);讀完就轉成型別,交給 lib 裡可以測試的部分。
//! 結果印到 stdout,提示與警告印到 stderr(可以把報告導到檔案,又看得到警告)。

use std::path::Path;
use std::process::Command as Process;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use config_loader::ConfigError;
use eval::add::{self, CaseOptions};
use eval::cli::{AddArgs, Cli, Command, CompareArgs, RunArgs, ScoreArgs};
use eval::config::{RawConfig, Settings};
use eval::run::{self, Estimate, ExecOptions, Price, RunConfig, Summary};
use eval::score::{self, Judge, ScoreSetup, ScoreSummary};
use eval::store::RunDir;
use eval::{Dataset, compare};
use jiff::Timestamp;
use llm::{
    AliasTable, AnthropicClient, AnthropicConfig, EventStream, LlmError, LlmProvider,
    LlmProviderExt, Request,
};

// `anyhow::Result`:binary 只需要把錯誤和它的原因鏈印給人看(同 gateway)。
#[tokio::main]
async fn main() -> Result<()> {
    // `Cli::parse()`:參數不合法時,clap 印出錯誤與用法並結束(exit code 2),不會回到這裡。
    let cli = Cli::parse();
    // 找不到 .env 沒關係:真的環境變數一樣有效。
    dotenvy::dotenv().ok();
    match cli.command {
        Command::Run(args) => run_command(&load_settings(&cli.config)?, &args).await,
        Command::Score(args) => score_command(&load_settings(&cli.config)?, &args).await,
        // 比較只讀兩個執行目錄,不需要設定檔,也不呼叫任何模型。
        Command::Compare(args) => compare_command(&args),
        Command::Add(args) => add_command(&load_settings(&cli.config)?, &args),
    }
}

fn load_settings(path: &Path) -> Result<Settings> {
    let file = match config_loader::load_toml::<RawConfig>(path) {
        Ok(file) => file,
        Err(ConfigError::NotFound(path)) => bail!(
            "config file {} not found; create it with `cp eval.example.toml eval.toml`",
            path.display()
        ),
        Err(e) => return Err(e.into()),
    };
    Ok(Settings::from_config(
        &file,
        config_loader::default_data_dir,
    )?)
}

/// `Retry [ Alias [ AnthropicClient ] ]`,沒有 Record(D-033:eval 的題目不是真實使用,不能混進呼叫紀錄)。
fn build_provider(settings: &Settings) -> Result<Arc<dyn LlmProvider>> {
    let key = config_loader::secret_env("ANTHROPIC_API_KEY")
        .context("the API key is needed to call the model (put it in .env, see .env.example)")?;
    let client = AnthropicClient::new(AnthropicConfig {
        base_url: settings.base_url.clone(),
        ..AnthropicConfig::new(key)
    })?;
    Ok(Arc::new(
        client
            .with_alias(AliasTable::strict(settings.aliases.clone()))
            .with_retry(settings.retry),
    ))
}

/// 不呼叫任何模型的 provider:`--dry-run` 與只做確定性檢查時用,不需要 API key。
/// 真的被呼叫就是程式寫錯了,回傳錯誤(會被記成評審失敗),不會偷偷送出請求。
struct Offline;

impl LlmProvider for Offline {
    fn stream(&self, _request: Request) -> EventStream {
        Box::pin(futures::stream::once(async {
            Err(LlmError::InvalidRequest(
                "no model is called here (dry run or checks only)".into(),
            ))
        }))
    }
}

fn exec_options(
    settings: &Settings,
    concurrency: Option<usize>,
    budget: Option<f64>,
    price: Option<Price>,
) -> ExecOptions {
    ExecOptions {
        concurrency: concurrency.unwrap_or(settings.concurrency),
        timeout: settings.timeout,
        budget_usd: budget,
        price,
    }
}

async fn run_command(settings: &Settings, args: &RunArgs) -> Result<()> {
    if let Some(dir) = &args.resume {
        return resume_command(settings, dir, args).await;
    }
    // clap 已經保證沒有 `--resume` 時兩個都有;這裡再寫一次是為了不用 unwrap。
    let (Some(dataset), Some(model)) = (&args.dataset, &args.model) else {
        bail!("--dataset and --model are required for a new run");
    };
    settings.require_alias(model)?;
    let path = settings.find_dataset(dataset)?;
    let config = RunConfig {
        model: model.clone(),
        repeats: args.repeats,
        tags: args.tags.clone(),
        limit: args.limit,
        label: args.label.clone(),
    };
    let options = exec_options(
        settings,
        args.concurrency,
        args.budget,
        settings.price_of(model),
    );

    if args.dry_run {
        let dataset = Dataset::load(&path)?;
        let items = run::plan(&dataset, &config);
        print_estimate(
            "model",
            model,
            &run::estimate(&dataset, &items, options.price),
        );
        return Ok(());
    }
    let provider = build_provider(settings)?;
    let (dir, summary) = run::start(
        &settings.runs_dir,
        &path,
        config,
        options,
        provider.as_ref(),
        git_commit(),
        Timestamp::now(),
    )
    .await?;
    print_run(dir.path(), &summary);
    Ok(())
}

async fn resume_command(settings: &Settings, dir: &Path, args: &RunArgs) -> Result<()> {
    // 設定從 meta.json 讀回來(design §5);價格依這次執行的模型找。
    let meta = RunDir::open(dir)?.read_meta()?;
    let model = &meta.config.model;
    settings.require_alias(model)?;
    let options = exec_options(
        settings,
        args.concurrency,
        args.budget,
        settings.price_of(model),
    );

    if args.dry_run {
        let dataset = Dataset::load(&meta.dataset.path)?;
        let planned = run::plan(&dataset, &meta.config);
        let done = RunDir::open(dir)?.read_responses()?;
        let todo = run::pending(&dataset, &planned, &done, args.retry_errors);
        print_estimate(
            "model",
            model,
            &run::estimate(&dataset, &todo, options.price),
        );
        return Ok(());
    }
    let provider = build_provider(settings)?;
    let summary = run::resume(
        dir,
        options,
        args.retry_errors,
        provider.as_ref(),
        Timestamp::now(),
    )
    .await?;
    print_run(dir, &summary);
    Ok(())
}

async fn score_command(settings: &Settings, args: &ScoreArgs) -> Result<()> {
    let judge_alias = args.judge_alias(&settings.judge.alias);
    if let Some(alias) = judge_alias {
        settings.require_alias(alias)?;
    }
    let name = args.score_name(&settings.judge.alias);
    let price = judge_alias.and_then(|alias| settings.price_of(alias));
    // 只有真的要叫評審時才讀 API key:估價與只做檢查都不需要。
    let provider: Arc<dyn LlmProvider> = match judge_alias {
        Some(_) if !args.dry_run => build_provider(settings)?,
        _ => Arc::new(Offline),
    };
    let judge =
        judge_alias.map(|alias| Judge::new(provider.clone(), alias, &settings.judge.effort));
    let setup = ScoreSetup {
        name: &name,
        judge: judge.as_ref(),
        skip_on_check_failure: args.skip_on_check_failure(settings.judge.skip_on_check_failure),
    };

    if args.dry_run {
        let estimate = score::estimate(&args.run, &setup, price)?;
        let judge = judge_alias.unwrap_or("none");
        print_estimate(
            "judge",
            judge,
            &Estimate {
                calls: estimate.judge_calls,
                worst_case_usd: estimate.worst_case_usd,
            },
        );
        return Ok(());
    }
    let options = exec_options(settings, args.concurrency, args.budget, price);
    let report = score::score(&args.run, &setup, options, Timestamp::now()).await?;
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    print_score(&args.run, &name, &report.summary);
    Ok(())
}

fn compare_command(args: &CompareArgs) -> Result<()> {
    let name = match &args.scores {
        Some(name) => name.clone(),
        None => compare::common_score_name(&args.run_a, &args.run_b)?,
    };
    let a = compare::load(&args.run_a, &name)?;
    let b = compare::load(&args.run_b, &name)?;
    let report = compare::compare(&a, &b, args.allow_mismatch)?;
    print!("{report}");
    Ok(())
}

/// 從呼叫紀錄加一個案例到私有資料集;`--print` 只印出來、不寫檔(真實對話寫進檔案前,先讓人看一次)。
fn add_command(settings: &Settings, args: &AddArgs) -> Result<()> {
    let record = add::find_record(&settings.records_dir, args.record)?;
    let options = CaseOptions {
        id: args.id.clone(),
        tags: args.tags.clone(),
        rubric: args.rubric.clone(),
        keep_answer: args.keep_answer,
    };
    let case = add::case_from_record(&record, &options)?;
    // 先決定寫到哪裡(名稱不合法、跟公開資料集同名都在這裡擋下),`--print` 時也照樣檢查。
    let path = settings.private_dataset_path(&args.dataset)?;
    if args.print {
        println!("{}", serde_json::to_string_pretty(&case)?);
        eprintln!(
            "(not written; without --print it would be appended to {})",
            path.display()
        );
        return Ok(());
    }
    add::append_case(&path, &case)?;
    println!("added case `{}` to {}", case.id, path.display());
    Ok(())
}

/// 這次執行用的程式版本,寫進 meta.json。有沒 commit 的改動就加上 `-dirty`:那樣的結果重現不出來。
/// 不在 git repo 裡(或沒裝 git)就是 `None`,不是錯誤。
fn git_commit() -> Option<String> {
    let run = |args: &[&str]| {
        Process::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let commit = run(&["rev-parse", "HEAD"]).filter(|c| !c.is_empty())?;
    let dirty = run(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    Some(if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    })
}

fn usd(value: Option<f64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |v| format!("${v:.4}"))
}

fn print_estimate(role: &str, model: &str, estimate: &Estimate) {
    let worst = match estimate.worst_case_usd {
        Some(usd) => format!("${usd:.4}"),
        None => format!("unknown (no price for {role} `{model}` in [prices])"),
    };
    println!(
        "dry run: {} call(s), worst-case cost {worst}",
        estimate.calls
    );
    println!("(nothing was called; the worst case assumes every answer uses all of max_tokens)");
}

fn print_run(dir: &Path, s: &Summary) {
    println!("run: {}", dir.display());
    println!(
        "  planned {}, ok {}, errors {}, missing {}",
        s.planned, s.ok, s.errors, s.missing
    );
    println!("  cost {}", usd(s.cost_usd));
    if s.missing > 0 {
        eprintln!(
            "incomplete: {} call(s) were not made (budget or interruption); continue with `eval run --resume {}`",
            s.missing,
            dir.display()
        );
    }
    println!("next: eval score {}", dir.display());
}

fn print_score(dir: &Path, name: &str, s: &ScoreSummary) {
    println!("scores `{name}`: {}", dir.display());
    println!(
        "  pass {}, fail {}, incomplete {}, call errors {}, missing {}",
        s.pass, s.fail, s.incomplete, s.call_errors, s.missing
    );
    println!(
        "  judge calls {} (skipped {}), cost {}",
        s.judge_calls,
        s.judge_skipped,
        usd(s.cost_usd)
    );
    if s.judge_usage.cache_read_tokens > 0 {
        println!(
            "  prompt cache read {} token(s)",
            s.judge_usage.cache_read_tokens
        );
    }
    if s.missing > 0 {
        eprintln!("incomplete: run the same command again to score the rest");
    }
    if s.incomplete > 0 {
        eprintln!(
            "warning: {} response(s) have scorer errors; the scores are incomplete",
            s.incomplete
        );
    }
}
