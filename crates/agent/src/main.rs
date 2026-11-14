//! agent 的 composition root:解析命令列 → 讀設定 → 組裝 provider 與工具 → 一步一步執行 → 印出結果。
//!
//! ```text
//! cp agent.example.toml agent.toml
//! cargo run -p agent -- run "台北現在幾點?"
//! cargo run -p agent -- run "notes 裡有哪些待辦?" --root ~/notes
//! ```
//!
//! 整個 agent 只有這個檔案讀設定檔、環境變數與 `.env`(D-016)。
//! 回答印到 stdout,每一步的進度、摘要與警告印到 stderr(可以把回答導到檔案,又看得到過程)。

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use agent::cli::{Cli, Command, RunArgs};
use agent::config::{self, RawConfig, Settings};
use agent::tools::{Calculator, CurrentTime, ListDir, ReadFile, Root};
use agent::trajectory::{Event, TrajectoryWriter};
use agent::{
    Agent, AgentConfig, Clock, DEFAULT_SYSTEM, ErasedTool, Finished, Limit, Outcome, Step, Toolbox,
};
use anyhow::{Context, Result, bail};
use clap::Parser;
use config_loader::ConfigError;
use jiff::Timestamp;
use llm::{
    AliasTable, AnthropicClient, AnthropicConfig, DEFAULT_QUEUE_CAPACITY, JsonlWriter, LlmProvider,
    LlmProviderExt, ProviderMetadata, RecordSink,
};

/// 結束碼:0 = 回答了;1 = 執行了但沒有回答(拒答、撞到護欄、失敗);2 = 根本沒開始(設定、參數錯誤)。
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    dotenvy::dotenv().ok();
    let result = match cli.command {
        Command::Run(args) => match load_settings(&cli.config) {
            Ok(settings) => run(&settings, args).await,
            Err(e) => Err(e),
        },
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            // `{e:#}`:anyhow 的「一行印出整條原因鏈」格式。
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn load_settings(path: &Path) -> Result<Settings> {
    let file = match config_loader::load_toml::<RawConfig>(path) {
        Ok(file) => file,
        Err(ConfigError::NotFound(path)) => bail!(
            "config file {} not found; create it with `cp agent.example.toml agent.toml`",
            path.display()
        ),
        Err(e) => return Err(e.into()),
    };
    Ok(Settings::from_config(
        &file,
        config_loader::default_data_dir,
    )?)
}

async fn run(settings: &Settings, args: RunArgs) -> Result<ExitCode> {
    let model = args.model.clone().unwrap_or_else(|| settings.model.clone());
    if !settings.aliases.contains_key(&model) {
        let known: Vec<&str> = settings.aliases.keys().map(String::as_str).collect();
        bail!(
            "unknown model alias `{model}`; [aliases] in the config has: {}",
            known.join(", ")
        );
    }
    let system = match &args.system {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the system prompt {}", path.display()))?,
        None => DEFAULT_SYSTEM.to_string(),
    };
    let mut limits = settings.limits.clone();
    if let Some(turns) = args.max_turns {
        limits.max_turns = turns;
    }

    let mut tools: Vec<Box<dyn ErasedTool>> = vec![Box::new(Calculator), Box::new(CurrentTime)];
    // 沒給 `--root` 就不提供檔案工具(design §7):模型連「有這種工具」都不知道。
    if let Some(dir) = &args.root {
        let root = Root::new(dir)
            .with_context(|| format!("--root {} must be an existing directory", dir.display()))?;
        tools.push(Box::new(ReadFile::new(root.clone())));
        tools.push(Box::new(ListDir::new(root)));
    }
    let toolbox = Arc::new(Toolbox::new(tools)?);
    let tool_names: Vec<String> = toolbox.defs().into_iter().map(|d| d.name).collect();

    let (provider, records) = build_provider(settings, !args.no_record)?;
    let agent = Agent::new(
        provider,
        toolbox,
        AgentConfig {
            model: model.clone(),
            system: Some(system),
            max_tokens: settings.max_tokens,
            limits: limits.clone(),
            clock: Clock::System,
            provider_options: ProviderMetadata::default(),
        },
    )?;

    let started_at = Timestamp::now();
    let mut run = agent.start(args.task.clone());
    // 軌跡檔在第一次呼叫模型之前就建好:目錄有問題(權限太寬)時,一毛錢都還沒花就停下來。
    let mut trajectory = TrajectoryWriter::create(&settings.runs_dir, started_at, run.run_id())
        .with_context(|| {
            format!(
                "cannot create a trajectory in {}",
                settings.runs_dir.display()
            )
        })?;
    let mut write = |event: &Event| {
        // 軌跡是附帶的紀錄:寫入失敗不中斷回答,但要大聲說出來(同 D-021 的呼叫紀錄)。
        if let Err(e) = trajectory.write(event) {
            eprintln!("warning: could not write the trajectory: {e}");
        }
    };
    write(&Event::Start {
        run_id: run.run_id(),
        started_at,
        model,
        task: args.task,
        tools: tool_names,
        limits: limits.clone(),
    });
    while let Some(step) = run.next().await {
        print_step(&step);
        write(&Event::Step { step });
    }
    let finished = run.finish();
    let cost = config::cost(&finished.steps, &settings.prices);
    write(&Event::Finish {
        outcome: finished.outcome.clone(),
        usage: finished.usage,
        cost_usd: cost,
    });
    let path = trajectory.path().to_path_buf();

    // provider 鏈裡的 Record 也拿著一份 writer:先丟掉 agent(連同 provider),writer 才只剩我們這一份。
    drop(agent);
    if let Some(writer) = records {
        flush_records(writer).await;
    }

    print_summary(&finished, cost, &limits, &path);
    Ok(match &finished.outcome {
        Outcome::Answered { text } => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("{}", describe(other, &limits));
            ExitCode::from(1)
        }
    })
}

/// 組好的 provider 鏈,以及(有套 Record 時)紀錄的 writer:結束前要等它寫完。
type Provider = (Arc<dyn LlmProvider>, Option<Arc<JsonlWriter>>);

/// `Record [ Retry [ Alias [ AnthropicClient ] ] ]`(D-020 的順序)。agent 的對話是真實使用,
/// 預設寫進呼叫紀錄,是階段 2 的原料(D-045);`--no-record` 拿掉 Record。
fn build_provider(settings: &Settings, record: bool) -> Result<Provider> {
    let key = config_loader::secret_env("ANTHROPIC_API_KEY")
        .context("the API key is needed to call the model (put it in .env, see .env.example)")?;
    let client = AnthropicClient::new(AnthropicConfig {
        base_url: settings.base_url.clone(),
        ..AnthropicConfig::new(key)
    })?;
    let core: Arc<dyn LlmProvider> = Arc::new(
        client
            .with_alias(AliasTable::strict(settings.aliases.clone()))
            .with_retry(settings.retry),
    );
    if !record {
        return Ok((core, None));
    }
    // 啟動時就檢查紀錄目錄寫得進去(D-021);寫不進去就不開始。
    let writer =
        JsonlWriter::start(&settings.records_dir, DEFAULT_QUEUE_CAPACITY).with_context(|| {
            format!(
                "cannot write call records to {}",
                settings.records_dir.display()
            )
        })?;
    let writer = Arc::new(writer);
    let sink: Arc<dyn RecordSink> = writer.clone();
    Ok((Arc::new(core.with_record(sink)), Some(writer)))
}

/// 等背景的紀錄 writer 把佇列寫完。`shutdown` 會阻塞,所以放到 `spawn_blocking`(同 gateway)。
async fn flush_records(writer: Arc<JsonlWriter>) {
    match Arc::try_unwrap(writer) {
        Ok(writer) => {
            if tokio::task::spawn_blocking(move || writer.shutdown())
                .await
                .is_err()
            {
                eprintln!("warning: the call records writer did not shut down cleanly");
            }
        }
        Err(_) => {
            eprintln!("warning: the call records writer is still in use; records may be lost")
        }
    }
}

/// 每一步印一行:呼叫了哪個工具、參數、結果的開頭。
fn print_step(step: &Step) {
    for call in &step.tool_calls {
        let marker = if call.is_error { "error: " } else { "" };
        eprintln!(
            "[{}] {} {} → {marker}{}",
            step.turn,
            call.name,
            call.args_json,
            preview(&call.result)
        );
    }
}

/// 結果的第一行,最多 120 個字元。
fn preview(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default();
    let mut short: String = first.chars().take(120).collect();
    if short.len() < first.len() || text.lines().nth(1).is_some() {
        short.push('…');
    }
    short
}

fn print_summary(finished: &Finished, cost: Option<f64>, limits: &agent::AgentLimits, path: &Path) {
    let tools: usize = finished.steps.iter().map(|s| s.tool_calls.len()).sum();
    let cost = cost.map_or("unknown".to_string(), |c| format!("${c:.4}"));
    eprintln!(
        "turns {}/{} · tool calls {tools} · tokens {} in / {} out · cost {cost}",
        finished.steps.len(),
        limits.max_turns,
        finished.usage.input_tokens
            + finished.usage.cache_read_tokens
            + finished.usage.cache_write_tokens,
        finished.usage.output_tokens,
    );
    eprintln!("trajectory: {}", path.display());
}

fn describe(outcome: &Outcome, limits: &agent::AgentLimits) -> String {
    match outcome {
        Outcome::Answered { .. } => "answered".into(),
        Outcome::Refused => "the model refused to answer".into(),
        Outcome::LimitReached { limit } => match limit {
            Limit::Turns => format!("stopped: reached the turn limit ({})", limits.max_turns),
            Limit::ToolCalls => format!(
                "stopped: the next turn would exceed the tool call limit ({})",
                limits.max_tool_calls
            ),
            Limit::Tokens => format!(
                "stopped: reached the token limit ({})",
                limits.max_total_tokens
            ),
            Limit::Deadline => format!(
                "stopped: reached the time limit ({} seconds)",
                limits.deadline_secs
            ),
        },
        Outcome::Failed { failure } => format!("failed: {failure}"),
        Outcome::Stopped => "stopped before the end".into(),
    }
}
