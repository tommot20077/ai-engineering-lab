//! agent loop(design §5、D-042):呼叫模型 → 執行工具 → 結果接回去 → 再呼叫,直到模型回答或撞到護欄。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use llm::{
    Content, LlmProvider, Message, ProviderMetadata, Request, Response, Role, StopKind, ToolDef,
    Usage,
};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, timeout, timeout_at};
use uuid::Uuid;

use crate::limits::AgentLimits;
use crate::tool::{Clock, ToolContext, Toolbox};

/// 一個 agent 的設定:用哪個模型、system prompt、護欄。Harness² 說的「方法」。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 模型名稱(通常是別名,由 provider 鏈裡的 `Alias` 換成真的 id)。
    pub model: String,
    pub system: Option<String>,
    /// 每一輪模型回答的 token 上限。
    pub max_tokens: u32,
    pub limits: AgentLimits,
    /// 交給工具的時鐘(eval 用固定時間)。
    pub clock: Clock,
    /// 各家專屬的設定(例如 effort),原樣放進每一輪的請求。
    pub provider_options: ProviderMetadata,
    /// 有副作用的工具要不要先問人(D-056)。
    pub approval: ApprovalPolicy,
}

/// 有副作用的工具(`Tool::needs_approval`)執行之前要不要停下來問。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// 停下來:`next()` 回傳 `RunEvent::NeedsApproval`,呼叫端 `resolve()` 之後才繼續。預設。
    Ask,
    /// 不問,直接執行(CLI 的 `--yes`、eval)。
    AutoApprove,
}

impl AgentConfig {
    pub fn new(model: impl Into<String>) -> AgentConfig {
        AgentConfig {
            model: model.into(),
            system: None,
            max_tokens: 4096,
            limits: AgentLimits::default(),
            clock: Clock::System,
            provider_options: ProviderMetadata::default(),
            approval: ApprovalPolicy::Ask,
        }
    }
}

/// 設定不合法(寫入路徑 fail-loud:建立 agent 時就拒絕,不等到執行一半)。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("model must not be empty")]
    EmptyModel,
    #[error("max_tokens must be at least 1")]
    ZeroMaxTokens,
    #[error("agent limits must be at least 1: {}", .0.join(", "))]
    ZeroLimits(Vec<&'static str>),
}

/// 一個可以執行任務的 agent。本身不帶狀態,同一個 agent 可以執行很多次。
///
/// provider 是**借來的**(`&'p dyn LlmProvider`):provider 屬於組裝它的地方(CLI 的 `Arc`、eval 執行器借給每一題的參考),
/// agent 只是拿來用。`'p` 是那個借用的 lifetime:compiler 保證 agent 不會活得比 provider 久。
// 為什麼不用 `Arc<dyn LlmProvider>`:eval 的執行器手上只有 `&dyn LlmProvider`(它刻意不用 Arc),
// 借來的參考沒辦法變成「要永遠活著」的 `Arc`。改成借用,兩種呼叫者都能用。Java 對照:Java 只有一種參考,
// 不用想誰擁有;Rust 要在型別上寫出「借來的」,換來的是不可能在 provider 被丟掉之後還用它。
pub struct Agent<'p> {
    provider: &'p dyn LlmProvider,
    toolbox: Arc<Toolbox>,
    config: AgentConfig,
    /// 工具定義只算一次;每一輪都送一樣的內容,順序固定,prompt cache 才吃得到。
    tools: Vec<ToolDef>,
}

impl<'p> Agent<'p> {
    // 要套哪些 decorator(Record、Retry、Alias)由呼叫者決定(D-045),agent 不知道也不在乎。CLI 套 Record,eval 不套。
    pub fn new(
        provider: &'p dyn LlmProvider,
        toolbox: Arc<Toolbox>,
        config: AgentConfig,
    ) -> Result<Agent<'p>, ConfigError> {
        if config.model.trim().is_empty() {
            return Err(ConfigError::EmptyModel);
        }
        if config.max_tokens == 0 {
            return Err(ConfigError::ZeroMaxTokens);
        }
        let zero = config.limits.zero_fields();
        if !zero.is_empty() {
            return Err(ConfigError::ZeroLimits(zero));
        }
        let tools = toolbox.defs();
        Ok(Agent {
            provider,
            toolbox,
            config,
            tools,
        })
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// 開始一次執行。還沒呼叫模型;呼叫者用 `next()` 一步一步拉。
    // 回傳的 `Run<'_>` 借用了 `self`(`'_` 是那個借用的 lifetime):Run 活著的時候 agent 不能被丟掉。
    // Java 沒有這種限制,也就沒有這種保證。
    pub fn start(&self, task: impl Into<String>) -> Run<'_> {
        Run {
            agent: self,
            run_id: Uuid::now_v7(),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: task.into(),
                    meta: ProviderMetadata::default(),
                }],
            }],
            steps: Vec::new(),
            usage: Usage::default(),
            tool_calls: 0,
            call_ids: HashSet::new(),
            used: Duration::ZERO,
            since: Instant::now(),
            waiting: None,
            outcome: None,
        }
    }

    /// 一次跑完,不需要一步一步看的時候用。
    ///
    /// 沒有人可以問:`ApprovalPolicy::Ask` 時,要核准的呼叫**全部拒絕**(fail-closed),模型會收到「使用者拒絕」。
    /// 要讓它們執行就用 `ApprovalPolicy::AutoApprove`(eval 就是這樣)。
    pub async fn run(&self, task: impl Into<String>) -> Finished {
        let mut run = self.start(task);
        while let Some(event) = run.next().await {
            if let RunEvent::NeedsApproval(pending) = event {
                let deny_all = pending
                    .into_iter()
                    .map(|a| (a.call_id, Decision::Deny))
                    .collect();
                // 剛剛才拿到的清單,一定對得上;萬一對不上,`next()` 會再問一次,不會執行任何東西。
                let _ = run.resolve(deny_all);
            }
        }
        run.finish()
    }
}

/// 一次執行。狀態(對話、計數器、截止時間)和護欄都在這裡面,呼叫者只能拉下一步。
// `&'a Agent<'a>`:Run 借 agent 的時間('a)不會比 agent 借 provider 的時間長。寫成同一個 `'a` 就夠了:
// `Agent<'p>` 對 `'p` 是 covariant(活得久的可以當成活得短的用),compiler 會自動把 `'p` 縮成 `'a`。
pub struct Run<'a> {
    agent: &'a Agent<'a>,
    run_id: Uuid,
    messages: Vec<Message>,
    steps: Vec<Step>,
    usage: Usage,
    tool_calls: u32,
    /// 這次執行出現過的工具呼叫 id;重複就是協定出錯(之後的接續也靠 id 不重複)。
    call_ids: HashSet<String>,
    /// 到 `since` 為止已經用掉的時間。總時間上限只算「真的在跑」的時間:等核准的時間不算(D-057)。
    used: Duration,
    /// 這一段開始跑的時間(開始、或核准之後接著跑)。
    since: Instant,
    /// 停下來等核准的那一輪。
    waiting: Option<Waiting>,
    outcome: Option<Outcome>,
}

/// `next()` 交出來的東西。
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// 完成一輪。
    Step(Step),
    /// 這一輪有要核准的工具呼叫,停下來了。呼叫端用 `resolve()` 回答之後再 `next()`;
    /// 沒回答就 `next()`,會再拿到同一份清單(什麼都不會執行)。
    NeedsApproval(Vec<Approval>),
}

/// 一個等核准的工具呼叫。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub call_id: String,
    pub tool: String,
    /// 模型給的原始參數(不可信,顯示前要整理)。
    pub args_json: String,
    /// 工具自己說明的「要做什麼」(`Tool::describe`;同樣不可信)。
    pub description: String,
}

/// 對一個等核准的呼叫的回答。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    /// 不執行;模型收到「使用者拒絕」的錯誤結果,可以換個做法。
    Deny,
}

/// `resolve()` 的回答對不上等核准的清單。狀態不變,可以重新回答。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("the run is not waiting for approval")]
    NotWaiting,
    #[error("no pending tool call has id {0:?}")]
    Unknown(String),
    #[error("tool call {0:?} was answered twice")]
    Duplicate(String),
    #[error("tool call {0:?} has no answer")]
    Missing(String),
}

/// 停下來等核准的一輪:模型的回應已經拿到(用量也算進去了),工具還沒執行。
struct Waiting {
    step: Step,
    calls: Vec<Call>,
    approvals: Vec<Approval>,
    /// `resolve()` 之後才有:每個要核准的呼叫的回答。
    decisions: Option<HashMap<String, Decision>>,
}

/// 一步 = 一次模型呼叫 + 這一輪的工具。內容完整,可以寫成軌跡、給 eval 打分。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// 第幾輪(從 1 算)。
    pub turn: u32,
    /// 模型的完整回應。
    pub response: Response,
    pub latency_ms: u64,
    /// 這一輪執行的工具;沒有執行(回答了、撞到護欄)就是空的。
    pub tool_calls: Vec<ToolCallRecord>,
}

/// 一次工具呼叫:參數、結果、花了多久。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub call_id: String,
    pub name: String,
    pub args_json: String,
    /// 交回模型的內容(已截斷)。
    pub result: String,
    pub is_error: bool,
    pub duration_ms: u64,
}

/// 執行怎麼結束的。
// `tag = "kind"`:JSON 長這樣 `{"kind": "limit_reached", "limit": "turns"}`(跟 llm 的 `Content` 同一種寫法)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Answered {
        text: String,
    },
    Refused,
    LimitReached {
        limit: Limit,
    },
    Failed {
        failure: AgentFailure,
    },
    /// 呼叫者在結束之前就不再拉下一步(包括停在等核准的時候)。
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    Turns,
    ToolCalls,
    Tokens,
    Deadline,
}

/// 失敗的原因。停止原因不認得、或前後矛盾,一律當失敗,不猜(D-042)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum AgentFailure {
    /// 模型呼叫失敗(Retry 之後)。
    #[error("model call failed: {message}")]
    Model { message: String },
    /// 撞到 `max_tokens`:回答被截斷,工具呼叫的參數可能不完整。
    #[error("the answer was cut off at max_tokens")]
    Truncated,
    #[error("unexpected stop reason {raw:?}")]
    UnexpectedStop { raw: String },
    #[error("the model stopped for tool use but called no tool")]
    ToolUseWithoutCalls,
    #[error("the model called tools but did not stop for tool use")]
    CallsWithoutToolUse,
    #[error("tool call id {id:?} was used twice")]
    DuplicateCallId { id: String },
}

/// 一次執行的完整結果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finished {
    pub run_id: Uuid,
    pub outcome: Outcome,
    pub steps: Vec<Step>,
    /// 每一輪用量的加總。
    pub usage: Usage,
}

/// 模型要的一次工具呼叫。
#[derive(Debug, Clone)]
struct Call {
    id: String,
    name: String,
    args_json: String,
}

impl Run<'_> {
    pub fn run_id(&self) -> Uuid {
        self.run_id
    }

    /// 走一步:呼叫模型一次,需要的話執行這一輪的工具;有要核准的工具時先停下來(`NeedsApproval`)。
    /// 結束之後回 `None`。
    pub async fn next(&mut self) -> Option<RunEvent> {
        if self.outcome.is_some() {
            return None;
        }
        // `take()`:把 Option 裡的值拿出來、原地留下 None。Java 沒有對應的寫法,因為 Java 不追蹤「誰擁有這個值」;
        // 這裡要把 `Waiting` 整個搬出來(它的欄位要交給別的函式),所以得先從 `self` 身上拿走。
        if let Some(waiting) = self.waiting.take() {
            return Some(match waiting.decisions {
                None => {
                    let pending = waiting.approvals.clone();
                    self.waiting = Some(waiting);
                    RunEvent::NeedsApproval(pending)
                }
                Some(decisions) => {
                    let step = self
                        .finish_turn(waiting.step, waiting.calls, &decisions)
                        .await;
                    self.steps.push(step.clone());
                    RunEvent::Step(step)
                }
            });
        }
        match self.step().await {
            Turn::Done(step, outcome) => {
                self.outcome = outcome;
                // `inspect`:有值的時候順便做一件事(存一份),值照樣往外傳。
                step.inspect(|s| self.steps.push(s.clone()))
                    .map(RunEvent::Step)
            }
            Turn::Wait(waiting) => {
                // 時間停在這裡:等核准的時間不算(D-057)。
                self.used += self.since.elapsed();
                let pending = waiting.approvals.clone();
                self.waiting = Some(waiting);
                Some(RunEvent::NeedsApproval(pending))
            }
        }
    }

    /// 正在等回答的呼叫;沒有在等(或已經回答了)就是空的。
    pub fn pending(&self) -> &[Approval] {
        match &self.waiting {
            Some(w) if w.decisions.is_none() => &w.approvals,
            _ => &[],
        }
    }

    /// 回答等核准的呼叫:每一個都要剛好回答一次(依 call id)。對不上就回錯誤、什麼都不改。
    /// 回答之後時間接著算,下一次 `next()` 執行這一輪(核准的執行、拒絕的回報給模型)。
    pub fn resolve(&mut self, decisions: Vec<(String, Decision)>) -> Result<(), ResolveError> {
        let waiting = match &mut self.waiting {
            Some(waiting) if waiting.decisions.is_none() => waiting,
            _ => return Err(ResolveError::NotWaiting),
        };
        let mut answers = HashMap::new();
        for (call_id, decision) in decisions {
            if !waiting.approvals.iter().any(|a| a.call_id == call_id) {
                return Err(ResolveError::Unknown(call_id));
            }
            if answers.insert(call_id.clone(), decision).is_some() {
                return Err(ResolveError::Duplicate(call_id));
            }
        }
        if let Some(missing) = waiting
            .approvals
            .iter()
            .find(|a| !answers.contains_key(&a.call_id))
        {
            return Err(ResolveError::Missing(missing.call_id.clone()));
        }
        waiting.decisions = Some(answers);
        self.since = Instant::now();
        Ok(())
    }

    /// 結束這次執行,拿到完整結果。還沒結束就呼叫,結果是 `Stopped`;停在等核准時,那一輪(模型的回應,沒有工具結果)也算進去。
    // 參數是 `self`(不是 `&self`):呼叫之後 Run 就被用掉了,不能再 `next()`。Java 只能靠文件提醒。
    pub fn finish(mut self) -> Finished {
        if let Some(waiting) = self.waiting.take() {
            self.steps.push(waiting.step);
        }
        Finished {
            run_id: self.run_id,
            outcome: self.outcome.unwrap_or(Outcome::Stopped),
            steps: self.steps,
            usage: self.usage,
        }
    }

    async fn step(&mut self) -> Turn {
        let agent = self.agent;
        let limits = &agent.config.limits;
        let limit = |limit| Some(Outcome::LimitReached { limit });

        // 呼叫模型之前先檢查護欄。token 只能事後知道,所以最多超出一輪。
        if self.steps.len() >= limits.max_turns as usize {
            return Turn::Done(None, limit(Limit::Turns));
        }
        if total_tokens(&self.usage) >= limits.max_total_tokens {
            return Turn::Done(None, limit(Limit::Tokens));
        }
        let budget = Duration::from_secs(limits.deadline_secs).saturating_sub(self.used);
        let deadline = self.since + budget;

        let request = Request {
            model: agent.config.model.clone(),
            system: agent.config.system.clone(),
            messages: self.messages.clone(),
            tools: agent.tools.clone(),
            max_tokens: agent.config.max_tokens,
            provider_options: agent.config.provider_options.clone(),
        };
        let begun = Instant::now();
        // `timeout_at`:到截止時間還沒回來,就丟掉這個 future(等於取消請求)。
        let response = match timeout_at(deadline, agent.provider.complete(request)).await {
            Err(_elapsed) => return Turn::Done(None, limit(Limit::Deadline)),
            Ok(Err(e)) => {
                let failure = AgentFailure::Model {
                    message: e.to_string(),
                };
                return Turn::Done(None, Some(Outcome::Failed { failure }));
            }
            Ok(Ok(response)) => response,
        };
        add_usage(&mut self.usage, response.usage);

        let calls = tool_calls(&response);
        let mut step = Step {
            turn: u32::try_from(self.steps.len() + 1).unwrap_or(u32::MAX),
            latency_ms: millis(begun),
            response,
            tool_calls: Vec::new(),
        };
        let failed = |failure| Some(Outcome::Failed { failure });
        // 窮舉每一種停止原因:之後 llm 多了一種,compiler 會指出這裡沒處理。
        let outcome = match step.response.stop.kind {
            StopKind::EndTurn if calls.is_empty() => Some(Outcome::Answered {
                text: answer_text(&step.response),
            }),
            StopKind::EndTurn => failed(AgentFailure::CallsWithoutToolUse),
            StopKind::ToolUse if calls.is_empty() => failed(AgentFailure::ToolUseWithoutCalls),
            StopKind::ToolUse => {
                if let Some(id) = self.first_repeated_id(&calls) {
                    failed(AgentFailure::DuplicateCallId { id })
                } else if self.tool_calls as usize + calls.len() > limits.max_tool_calls as usize {
                    // 要嘛整輪執行,要嘛停,不做一半。
                    limit(Limit::ToolCalls)
                } else {
                    let approvals = self.approvals(&calls).await;
                    if !approvals.is_empty() {
                        // 整輪一起等:沒有副作用的工具也等核准之後才跟著執行,暫停的狀態只有「還沒執行」一種。
                        return Turn::Wait(Waiting {
                            step,
                            calls,
                            approvals,
                            decisions: None,
                        });
                    }
                    step = self.finish_turn(step, calls, &HashMap::new()).await;
                    None
                }
            }
            StopKind::MaxTokens => failed(AgentFailure::Truncated),
            StopKind::Refusal => Some(Outcome::Refused),
            StopKind::PauseTurn | StopKind::StopSequence | StopKind::Unknown => {
                failed(AgentFailure::UnexpectedStop {
                    raw: step.response.stop.raw.clone(),
                })
            }
        };
        Turn::Done(Some(step), outcome)
    }

    /// 這一輪裡要核准的呼叫(`ApprovalPolicy::Ask` 而且工具有副作用)。參數太大的不算:它們不會被解析、也不會執行。
    async fn approvals(&self, calls: &[Call]) -> Vec<Approval> {
        let agent = self.agent;
        if agent.config.approval == ApprovalPolicy::AutoApprove {
            return Vec::new();
        }
        let mut approvals = Vec::new();
        for call in calls {
            if !agent.toolbox.needs_approval(&call.name)
                || call.args_json.len() > agent.config.limits.max_args_bytes
            {
                continue;
            }
            let ctx = self.context(&call.id);
            approvals.push(Approval {
                call_id: call.id.clone(),
                tool: call.name.clone(),
                args_json: call.args_json.clone(),
                description: agent
                    .toolbox
                    .describe(&call.name, &call.args_json, &ctx)
                    .await,
            });
        }
        approvals
    }

    fn context(&self, call_id: &str) -> ToolContext {
        ToolContext {
            run_id: self.run_id,
            call_id: call_id.to_owned(),
            clock: self.agent.config.clock,
        }
    }

    /// 執行這一輪的工具(`decisions` 裡是 `Deny` 的不執行),把回應與結果接進對話。
    async fn finish_turn(
        &mut self,
        mut step: Step,
        calls: Vec<Call>,
        decisions: &HashMap<String, Decision>,
    ) -> Step {
        let denied: HashSet<String> = decisions
            .iter()
            .filter(|(_, d)| **d == Decision::Deny)
            .map(|(id, _)| id.clone())
            .collect();
        step.tool_calls = self.execute(calls, &denied).await;
        self.messages.push(Message {
            role: Role::Assistant,
            // 整則內容原封不動接回去:thinking 區塊(含簽章)也在裡面,Anthropic 規定用工具時要這樣做。
            content: step.response.content.clone(),
        });
        self.messages.push(Message {
            role: Role::User,
            // 這一輪的全部結果放在同一則訊息,順序跟呼叫一樣。
            content: step
                .tool_calls
                .iter()
                .map(|r| Content::ToolResult {
                    call_id: r.call_id.clone(),
                    content: r.result.clone(),
                    is_error: r.is_error,
                })
                .collect(),
        });
        step
    }

    /// 同一輪裡重複、或跟之前的輪重複的 id。
    fn first_repeated_id(&self, calls: &[Call]) -> Option<String> {
        let mut seen = HashSet::new();
        calls
            .iter()
            .find(|c| self.call_ids.contains(&c.id) || !seen.insert(c.id.as_str()))
            .map(|c| c.id.clone())
    }

    /// 並行執行,結果照呼叫的順序排回去。`denied` 裡的不執行,結果是「使用者拒絕」。
    async fn execute(&mut self, calls: Vec<Call>, denied: &HashSet<String>) -> Vec<ToolCallRecord> {
        let agent = self.agent;
        let run_id = self.run_id;
        self.tool_calls += u32::try_from(calls.len()).unwrap_or(u32::MAX);
        self.call_ids.extend(calls.iter().map(|c| c.id.clone()));
        // `buffer_unordered(n)`:最多同時跑 n 個,誰先好誰先出;每個結果帶著原本的位置,最後排回呼叫的順序。
        // 不用 `buffered(n)`(照順序交出):它把「已經好了、但還沒輪到交出」的結果也算進 n 個名額,
        // 前面一個慢的工具會擋住後面的工具開始跑(head-of-line blocking)。
        let mut records: Vec<(usize, ToolCallRecord)> = futures::stream::iter(calls)
            .enumerate()
            .map(|(i, call)| async move {
                let record = if denied.contains(&call.id) {
                    denied_record(call)
                } else {
                    execute_one(agent, run_id, call).await
                };
                (i, record)
            })
            .buffer_unordered(agent.config.limits.max_parallel)
            .collect()
            .await;
        records.sort_by_key(|(i, _)| *i);
        records.into_iter().map(|(_, record)| record).collect()
    }
}

async fn execute_one(agent: &Agent<'_>, run_id: Uuid, call: Call) -> ToolCallRecord {
    let limits = &agent.config.limits;
    let begun = Instant::now();
    let (result, is_error) = if call.args_json.len() > limits.max_args_bytes {
        // 先看長度再解析:參數是不可信輸入,太大的連解析都不做。
        (
            format!("arguments are larger than {} bytes", limits.max_args_bytes),
            true,
        )
    } else {
        let ctx = ToolContext {
            run_id,
            call_id: call.id.clone(),
            clock: agent.config.clock,
        };
        let limit = Duration::from_secs(limits.tool_timeout_secs);
        match timeout(limit, agent.toolbox.call(&call.name, &call.args_json, &ctx)).await {
            Ok(Ok(output)) => (output, false),
            // 工具的錯誤一律交回模型(`is_error: true`),讓它自己修正。
            Ok(Err(e)) => (e.to_string(), true),
            Err(_elapsed) => (
                format!(
                    "the tool did not finish within {} seconds",
                    limits.tool_timeout_secs
                ),
                true,
            ),
        }
    };
    ToolCallRecord {
        call_id: call.id,
        name: call.name,
        args_json: call.args_json,
        result: truncate(result, limits.max_result_bytes),
        is_error,
        duration_ms: millis(begun),
    }
}

/// 被拒絕的呼叫:沒有執行,告訴模型是使用者拒絕的(它可以換個做法、或問使用者)。
fn denied_record(call: Call) -> ToolCallRecord {
    ToolCallRecord {
        call_id: call.id,
        name: call.name,
        args_json: call.args_json,
        result: DENIED.to_owned(),
        is_error: true,
        duration_ms: 0,
    }
}

/// 被拒絕時交給模型的結果。
pub const DENIED: &str = "the user denied this tool call; it was not executed";

/// `step()` 的結果:這一輪做完了,或停下來等核准。
enum Turn {
    Done(Option<Step>, Option<Outcome>),
    Wait(Waiting),
}

fn tool_calls(response: &Response) -> Vec<Call> {
    response
        .content
        .iter()
        .filter_map(|c| match c {
            Content::ToolCall {
                id,
                name,
                args_json,
                ..
            } => Some(Call {
                id: id.clone(),
                name: name.clone(),
                args_json: args_json.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// 回答 = 這一輪所有文字段落接起來(thinking 不算)。
fn answer_text(response: &Response) -> String {
    response
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// 超過上限就截斷,切在 UTF-8 字元的邊界上,最後註明截掉多少。
fn truncate(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    // `floor_char_boundary(max)`:不超過 max 的最後一個字元邊界。直接切在 max 可能切在中文字的中間,
    // `String::truncate` 會 panic(Rust 的字串保證永遠是合法的 UTF-8)。
    let cut = text.floor_char_boundary(max);
    let omitted = text.len() - cut;
    text.truncate(cut);
    text.push_str(&format!("\n[truncated: {omitted} bytes omitted]"));
    text
}

fn total_tokens(usage: &Usage) -> u64 {
    usage.input_tokens + usage.output_tokens + usage.cache_read_tokens + usage.cache_write_tokens
}

fn add_usage(total: &mut Usage, more: Usage) {
    total.input_tokens += more.input_tokens;
    total.output_tokens += more.output_tokens;
    total.cache_read_tokens += more.cache_read_tokens;
    total.cache_write_tokens += more.cache_write_tokens;
}

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
