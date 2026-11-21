//! agent 的 composition root:解析命令列 → 讀設定 → 組裝 provider 與工具 → 一步一步執行 → 印出結果。
//!
//! ```text
//! cp agent.example.toml agent.toml
//! cargo run -p agent -- run "台北現在幾點?"
//! cargo run -p agent -- run "notes 裡有哪些待辦?" --root ~/notes
//! cargo run -p agent -- run "把待辦整理一下" --root ~/notes --allow-write   # 每次寫入都會問
//! cargo run -p agent -- resume <run id>                                     # 接續存檔的執行
//! ```
//!
//! 整個 agent 只有這個檔案讀設定檔、環境變數與 `.env`(D-016)。
//! 回答印到 stdout,每一步的進度、摘要與警告印到 stderr(可以把回答導到檔案,又看得到過程)。

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use agent::cli::{Answer, Cli, Command, ResumeArgs, RunArgs, answer, terminal_safe};
use agent::config::{self, McpServer, McpTransport, RawConfig, Settings};
use agent::saved::{self, RunPlan, SUSPENDED_DIR, SavedRun};
use agent::tools::mcp::CallTool;
use agent::tools::{
    Calculator, CurrentTime, EditFile, ListDir, ReadFile, Root, WriteFile, select_mcp_tools,
};
use agent::trajectory::{ApprovalRecord, Event, TrajectoryWriter};
use agent::{
    Agent, AgentConfig, AgentLimits, Approval, ApprovalPolicy, Clock, DEFAULT_SYSTEM, Decision,
    ErasedTool, Finished, Limit, Outcome, Run, RunEvent, Step, Toolbox,
};
use anyhow::{Context, Result, bail};
use clap::Parser;
use config_loader::ConfigError;
use jiff::Timestamp;
use llm::{
    AliasTable, AnthropicClient, AnthropicConfig, DEFAULT_QUEUE_CAPACITY, JsonlWriter, LlmProvider,
    LlmProviderExt, ProviderMetadata, RecordSink,
};
use mcp::transport::{
    HttpConfig, HttpLimits, HttpTransport, StdioConfig, StdioLimits, StdioTransport, Transport,
};
use mcp::{ClientOptions, McpClient};

/// 存檔之後的結束碼(在等核准,還沒有答案)。
const EXIT_SAVED: u8 = 3;

/// 結束碼:0 = 回答了;1 = 執行了但沒有回答(拒答、撞到護欄、失敗);2 = 根本沒開始(設定、參數錯誤);
/// 3 = 停在等核准時存檔了(`agent resume <run id>` 接續)。
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    dotenvy::dotenv().ok();
    let result = match load_settings(&cli.config) {
        Ok(settings) => match cli.command {
            Command::Run(args) => run(&settings, args).await,
            Command::Resume(args) => resume(&settings, args).await,
        },
        Err(e) => Err(e),
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

/// `agent run`:把命令列整理成 `RunPlan`(存檔時存的就是它),從頭開始。
async fn run(settings: &Settings, args: RunArgs) -> Result<ExitCode> {
    let system = match &args.system {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the system prompt {}", path.display()))?,
        None => DEFAULT_SYSTEM.to_string(),
    };
    let root =
        match &args.root {
            // 存成絕對路徑:接續時的目前目錄可能不一樣。
            Some(dir) => Some(dir.canonicalize().with_context(|| {
                format!("--root {} must be an existing directory", dir.display())
            })?),
            None => None,
        };
    let plan = RunPlan {
        model: args.model.unwrap_or_else(|| settings.model.clone()),
        system,
        max_turns: args.max_turns,
        root,
        allow_write: args.allow_write,
        record: !args.no_record,
    };
    execute(settings, plan, Begin::New(args.task), args.yes).await
}

/// `agent resume`:讀存檔,用存檔裡的選項重建 agent,接著問核准。
async fn resume(settings: &Settings, args: ResumeArgs) -> Result<ExitCode> {
    let dir = settings.runs_dir.join(SUSPENDED_DIR);
    let saved = saved::load(&dir, args.run_id)?;
    let plan = saved.plan.clone();
    execute(settings, plan, Begin::Resume(Box::new(saved)), args.yes).await
}

/// 從哪裡開始。
enum Begin {
    New(String),
    // `Box`:`SavedRun` 很大(整段對話),enum 的大小是最大的那個 variant;裝箱之後 `Begin` 只有一個指標大。
    Resume(Box<SavedRun>),
}

/// 組裝 agent、執行到結束或存檔、收尾。`run` 與 `resume` 共用。
async fn execute(settings: &Settings, plan: RunPlan, begin: Begin, yes: bool) -> Result<ExitCode> {
    let model = plan.model.clone();
    if !settings.aliases.contains_key(&model) {
        let known: Vec<&str> = settings.aliases.keys().map(String::as_str).collect();
        bail!(
            "unknown model alias `{model}`; [aliases] in the config has: {}",
            known.join(", ")
        );
    }
    let mut limits = settings.limits.clone();
    if let Some(turns) = plan.max_turns {
        limits.max_turns = turns;
    }

    let mut tools: Vec<Box<dyn ErasedTool>> = vec![Box::new(Calculator), Box::new(CurrentTime)];
    // 沒給 `--root` 就不提供檔案工具(design §7):模型連「有這種工具」都不知道。
    if let Some(dir) = &plan.root {
        let root = Root::new(dir)
            .with_context(|| format!("--root {} must be an existing directory", dir.display()))?;
        tools.push(Box::new(ReadFile::new(root.clone())));
        tools.push(Box::new(ListDir::new(root.clone())));
        // 有副作用的工具要另外打開(D-056);打開之後每次呼叫還要核准(`--yes` 才不問)。
        if plan.allow_write {
            tools.push(Box::new(WriteFile::new(root.clone())));
            tools.push(Box::new(EditFile::new(root)));
        }
    }
    let (provider, records) = build_provider(settings, plan.record)?;
    // 所有 MCP server 在第一次呼叫模型之前連好、挑好工具;任何一個失敗就結束(結束碼 2),一毛錢都還沒花(design-part2 §6)。
    let mcp_clients = connect_mcp(&settings.mcp, &mut tools).await?;
    let toolbox = Arc::new(Toolbox::new(tools)?);
    let tool_names: Vec<String> = toolbox.defs().into_iter().map(|d| d.name).collect();

    let agent = Agent::new(
        provider.as_ref(),
        toolbox,
        AgentConfig {
            model: model.clone(),
            system: Some(plan.system.clone()),
            max_tokens: settings.max_tokens,
            limits: limits.clone(),
            clock: Clock::System,
            provider_options: ProviderMetadata::default(),
            approval: if yes {
                ApprovalPolicy::AutoApprove
            } else {
                ApprovalPolicy::Ask
            },
        },
    )?;

    let (mut run, mut trajectory) = match begin {
        Begin::New(task) => {
            let started_at = Timestamp::now();
            let run = agent.start(task.clone());
            // 軌跡檔在第一次呼叫模型之前就建好:目錄有問題(權限太寬)時,一毛錢都還沒花就停下來。
            let mut trajectory =
                TrajectoryWriter::create(&settings.runs_dir, started_at, run.run_id())
                    .with_context(|| {
                        format!(
                            "cannot create a trajectory in {}",
                            settings.runs_dir.display()
                        )
                    })?;
            record(
                &mut trajectory,
                &Event::Start {
                    run_id: run.run_id(),
                    started_at,
                    model,
                    task,
                    tools: tool_names,
                    limits: limits.clone(),
                    auto_approve: yes,
                },
            );
            (run, trajectory)
        }
        Begin::Resume(saved) => {
            let run_id = saved.suspended.run_id;
            // 先確認接得上(設定、工具一樣),再認領存檔:接不上的話存檔還在,改好設定可以再試。
            let run = agent
                .resume(saved.suspended)
                .await
                .context("cannot resume the run")?;
            let mut trajectory =
                TrajectoryWriter::append(&saved.trajectory).with_context(|| {
                    format!(
                        "cannot continue the trajectory {}",
                        saved.trajectory.display()
                    )
                })?;
            // 認領在執行任何東西之前:同一個存檔被接續兩次時,只有一個能繼續。
            saved::claim(&settings.runs_dir.join(SUSPENDED_DIR), run_id)?;
            record(
                &mut trajectory,
                &Event::Resume {
                    resumed_at: Timestamp::now(),
                },
            );
            eprintln!("resuming run {run_id}");
            (run, trajectory)
        }
    };

    let ending = drive(&mut run, &mut trajectory).await?;
    let saved_to = match ending {
        Ending::Finished => None,
        Ending::Save => Some(save(settings, &plan, &mut run, &mut trajectory)?),
    };
    let finished = run.finish();
    let cost = config::cost(&finished.steps, &settings.prices);
    if saved_to.is_none() {
        record(
            &mut trajectory,
            &Event::Finish {
                outcome: finished.outcome.clone(),
                usage: finished.usage,
                cost_usd: cost,
            },
        );
    }
    let path = trajectory.path().to_path_buf();

    // provider 鏈裡的 Record 也拿著一份 writer:先丟掉 agent(它借著 provider)和 provider 本身,writer 才只剩我們這一份。
    drop(agent);
    drop(provider);
    if let Some(writer) = records {
        flush_records(writer).await;
    }
    close_mcp(mcp_clients).await;

    if let Some(file) = saved_to {
        eprintln!(
            "saved while waiting for approval: {}\ncontinue with: agent resume {}",
            file.display(),
            finished.run_id
        );
        return Ok(ExitCode::from(EXIT_SAVED));
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

/// 執行怎麼停下來的。
enum Ending {
    Finished,
    /// 使用者在核准提示回答 `s`。
    Save,
}

/// 一步一步跑,遇到要核准的就問。
async fn drive(run: &mut Run<'_>, trajectory: &mut TrajectoryWriter) -> Result<Ending> {
    while let Some(event) = run.next().await {
        match event {
            RunEvent::Step(step) => {
                print_step(&step);
                record(trajectory, &Event::Step { step });
            }
            RunEvent::NeedsApproval(pending) => {
                let Some(decisions) = ask(pending).await? else {
                    return Ok(Ending::Save);
                };
                record(
                    trajectory,
                    &Event::Approval {
                        decisions: decisions.clone(),
                    },
                );
                run.resolve(
                    decisions
                        .into_iter()
                        .map(|d| (d.call_id, d.decision))
                        .collect(),
                )?;
            }
        }
    }
    Ok(Ending::Finished)
}

/// 存檔:`Run` 交出暫停的狀態,連同建立 agent 的選項寫進私有目錄。
fn save(
    settings: &Settings,
    plan: &RunPlan,
    run: &mut Run<'_>,
    trajectory: &mut TrajectoryWriter,
) -> Result<PathBuf> {
    let suspended = run
        .suspend()
        .context("the run is not waiting for approval")?;
    let saved = SavedRun {
        plan: plan.clone(),
        trajectory: trajectory.path().to_path_buf(),
        suspended,
    };
    let dir = settings.runs_dir.join(SUSPENDED_DIR);
    let file = saved::save(&dir, &saved)
        .with_context(|| format!("cannot save the run in {}", dir.display()))?;
    record(trajectory, &Event::Suspend { file: file.clone() });
    Ok(file)
}

/// 寫一行軌跡。軌跡是附帶的紀錄:寫入失敗不中斷回答,但要大聲說出來(同 D-021 的呼叫紀錄)。
fn record(trajectory: &mut TrajectoryWriter, event: &Event) {
    if let Err(e) = trajectory.write(event) {
        eprintln!("warning: could not write the trajectory: {e}");
    }
}

/// 在終端機上一個一個問(stderr 印說明、stdin 讀回答)。只有 `y` / `yes` 是核准;空白行、其他回答、
/// stdin 已經結束(例如被管線接上又關掉)一律是拒絕(fail-closed)。`s` / `save`:不再問,回傳 `None`(存檔)。
///
/// 讀 stdin 會卡住執行緒,所以放到 `spawn_blocking`:async runtime 的執行緒還要處理別的事(MCP 連線的背景 task)。
async fn ask(pending: Vec<Approval>) -> Result<Option<Vec<ApprovalRecord>>> {
    let decisions = tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let mut decisions = Vec::new();
        for approval in pending {
            eprintln!(
                "\n── approval needed: {} ({})",
                terminal_safe(&approval.tool),
                terminal_safe(&approval.call_id)
            );
            eprintln!("{}", terminal_safe(&approval.description));
            eprint!("allow? [y/N, s = save and stop] ");
            let mut line = String::new();
            let reply = match stdin.lock().read_line(&mut line) {
                Ok(read) if read > 0 => answer(&line),
                _ => Answer::Deny,
            };
            if line.is_empty() {
                // stdin 已經結束:換行補上,下一行輸出才不會接在提示後面。
                eprintln!();
            }
            let decision = match reply {
                Answer::Approve => Decision::Approve,
                Answer::Deny => Decision::Deny,
                Answer::Save => return None,
            };
            decisions.push(ApprovalRecord {
                call_id: approval.call_id,
                tool: approval.tool,
                decision,
            });
        }
        Some(decisions)
    })
    .await
    .context("the approval prompt failed")?;
    Ok(decisions)
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

/// stdio 的 MCP server 從 agent 繼承的環境變數(D-050):找得到其他程式、知道家目錄、語系。其他(API key 之類)一律不給。
/// 放在 binary:library 不讀環境變數(D-016)。
const MCP_INHERITED_ENV: [&str; 3] = ["PATH", "HOME", "LANG"];

/// 連上每個 `[[mcp]]`、依白名單挑出工具加進 `tools`。回傳 client,結束時關閉。
async fn connect_mcp(
    servers: &[McpServer],
    tools: &mut Vec<Box<dyn ErasedTool>>,
) -> Result<Vec<Arc<McpClient>>> {
    let mut clients = Vec::new();
    for server in servers {
        let context = || format!("MCP server `{}`", server.name);
        let transport: Box<dyn Transport> = match &server.transport {
            McpTransport::Stdio { command, args, env } => {
                let mut full_env: BTreeMap<String, String> = MCP_INHERITED_ENV
                    .iter()
                    .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
                    .collect();
                // 設定檔寫的蓋過繼承的。
                full_env.extend(env.clone());
                let config = StdioConfig {
                    command: command.clone(),
                    args: args.clone(),
                    env: full_env,
                };
                Box::new(
                    StdioTransport::spawn(&config, StdioLimits::default()).with_context(context)?,
                )
            }
            McpTransport::Http { url, token_env } => {
                let token = token_env
                    .as_deref()
                    .map(config_loader::secret_env)
                    .transpose()
                    .with_context(context)?;
                let config = HttpConfig {
                    url: url.clone(),
                    token,
                };
                Box::new(HttpTransport::new(&config, HttpLimits::default()).with_context(context)?)
            }
        };
        let client = McpClient::connect(transport, ClientOptions::default())
            .await
            .with_context(context)?;
        let list = match client.list_tools().await {
            Ok(list) => list,
            Err(e) => {
                client.close().await;
                return Err(anyhow::Error::new(e).context(context()));
            }
        };
        let client = Arc::new(client);
        let caller: Arc<dyn CallTool> = client.clone();
        match select_mcp_tools(&server.name, &list, &server.tools, caller) {
            Ok(selected) => {
                eprintln!(
                    "mcp: `{}` offers {} tool(s), using {}",
                    server.name,
                    list.tools.len(),
                    selected.len()
                );
                tools.extend(
                    selected
                        .into_iter()
                        .map(|t| Box::new(t) as Box<dyn ErasedTool>),
                );
            }
            Err(problems) => bail!(
                "MCP tools are not available:{}",
                problems
                    .iter()
                    .map(|p| format!("\n  - {p}"))
                    .collect::<String>()
            ),
        }
        clients.push(client);
    }
    Ok(clients)
}

/// 關閉每個 MCP 連線(stdio:關 stdin、等 server 結束)。agent 已經丟掉了,client 應該只剩這一份;
/// 還有人拿著的話就算了,子行程在 drop 時一樣會被收掉(`kill_on_drop`)。
async fn close_mcp(clients: Vec<Arc<McpClient>>) {
    for client in clients {
        if let Ok(client) = Arc::try_unwrap(client) {
            client.close().await;
        }
    }
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

fn print_summary(finished: &Finished, cost: Option<f64>, limits: &AgentLimits, path: &Path) {
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

fn describe(outcome: &Outcome, limits: &AgentLimits) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_is_the_first_line_cut_at_120_characters() {
        assert_eq!(preview("42"), "42");
        assert_eq!(preview(""), "");
        // 剛好 120 個字元(中文,每個 3 bytes):不加 `…`。比 bytes 會把它當成太長。
        let exact = "中".repeat(120);
        assert_eq!(preview(&exact), exact);
        // 121 個字元:切到 120,加 `…`。
        assert_eq!(preview(&"中".repeat(121)), format!("{}…", "中".repeat(120)));
        // 第一行很短,但後面還有:加 `…`。
        assert_eq!(preview("1\tfirst\n2\tsecond"), "1\tfirst…");
    }
}
