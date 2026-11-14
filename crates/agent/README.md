# agent

讓模型能「做事」:模型說要用哪個工具、參數是什麼,程式去執行,把結果交回模型,直到模型給出答案。
核心機制自己寫,不套 AI 框架。設計與取捨見 [`design.md`](../../docs/modules/003-agent-runtime/design.md)(D-041 ~ D-045)。

```
任務 ─→ 呼叫模型 ─→ 停止原因?
           ↑          ├─ end_turn ──────→ 回答
           │          ├─ tool_use ──→ 執行工具(並行、逾時、截斷)─┐
           │          └─ 其他 ───────→ 失敗(不猜)                 │
           └──────────── 工具結果接回對話 ←──────────────────────────┘
        每一輪之前:輪數、工具呼叫次數、token 合計、總時間,任何一項到上限就停(引擎自己的護欄)
```

這是 003 的第一部分。第二部分是 MCP client(把 MCP server 的工具接到同一個介面),第三部分是沙箱(寫檔、跑程式的工具)。

## 快速開始

不需要 API key,先看一次 agent loop 的流程(「模型」是照劇本回答的假 provider,工具是真的):

```bash
cargo run -p agent --example offline
```

真的呼叫 Claude:

```bash
# 在 repo 根目錄
cp .env.example .env                 # 填 ANTHROPIC_API_KEY
cp agent.example.toml agent.toml     # 想看費用就填 [prices];其他預設值就能用

cargo run -p agent --release -- run "台北現在幾點?離 2027 年 1 月 1 日還有幾天?"
cargo run -p agent --release -- run "notes 裡有哪些待辦?" --root ~/notes --model cheap
```

回答印到 stdout,每一步(呼叫了哪個工具、結果)和摘要印到 stderr:

```
[1] current_time {"timezone": "Asia/Taipei"} → 2026-11-14T10:30:00+08:00[Asia/Taipei], Saturday
[2] calculator {"expression": "31 + 17"} → 48
turns 3/10 · tool calls 2 · tokens 4210 in / 160 out · cost $0.0100
trajectory: ~/.local/share/ai-engineering-lab/agent/runs/20261114T023000Z_019a….jsonl
台北現在是 11 月 14 日(星期六)上午 10:30,離 2027 年 1 月 1 日還有 48 天。
```

結束碼:`0` 回答了;`1` 執行了但沒有回答(拒答、撞到護欄、失敗);`2` 根本沒開始(設定、參數、API key)。

## 指令

```
agent run "問題" [--model <別名>] [--root <目錄>] [--system <檔案>] [--max-turns <N>] [--no-record] [--config agent.toml]
```

| 參數 | 說明 |
|---|---|
| `--model` | `[aliases]` 裡的別名;預設 `[agent] model` |
| `--root` | 檔案工具(`read_file`、`list_dir`)能讀的目錄;沒給就不提供這兩個工具 |
| `--system` | 換掉內建的 system prompt(`src/prompt.rs`) |
| `--max-turns` | 這次執行的輪數上限,覆寫 `[limits] max_turns` |
| `--no-record` | 這次的模型呼叫不寫進呼叫紀錄 |

## 內建工具

都沒有副作用,不需要沙箱(D-044)。

| 工具 | 做什麼 | 安全重點 |
|---|---|---|
| `calculator` | `+ - * / % ^`、括號、負號、`sqrt abs ln log10 floor ceil round`、`pi e` | 自己寫的 lexer + Pratt parser,不用 eval;算式 1024 bytes、巢狀 64 層;每一步都要是有限的數 |
| `current_time` | 現在的時間與星期幾,可指定 IANA 時區 | 時區名稱先檢查形狀再查表;時鐘可以固定(測試、eval) |
| `read_file` | 讀文字檔,帶行號,可用 `offset`、`limit` 分段 | 只收相對路徑;解開 symlink 後必須還在根目錄裡;拒絕 `.` 開頭的路徑(`.env`、`.git`);256 KiB、UTF-8 |
| `list_dir` | 列出目錄(排序、目錄加 `/`、不列隱藏檔) | 跟 `read_file` 同一套路徑規則;最多 500 項 |

## 護欄與設定

全部在 `agent.toml`(範例 [`agent.example.toml`](../../agent.example.toml)),打錯欄位名稱會拒絕執行。

| `[limits]` | 預設 | 到上限時 |
|---|---|---|
| `max_turns` | 10 | 停止 |
| `max_tool_calls` | 30 | 下一輪會超過就整輪不執行、停止 |
| `max_parallel` | 4 | 同一輪的工具最多同時跑幾個 |
| `max_total_tokens` | 200,000 | 停止(每輪都重送整段對話,這是成本的硬上限;只能事後知道,最多超出一輪) |
| `deadline_secs` | 300 | 正在進行的模型呼叫被取消、停止 |
| `tool_timeout_secs` | 30 | 那次工具呼叫變成錯誤結果,執行繼續 |
| `max_result_bytes` | 32 KiB | 截斷(切在 UTF-8 邊界),註明截掉多少 |
| `max_args_bytes` | 64 KiB | 不解析,直接回錯誤結果 |

每一項至少是 1:可以調整,不能關掉。

## 資料存在哪裡

| 資料 | 位置 | 內容 |
|---|---|---|
| 呼叫紀錄 | 預設跟 gateway 同一個目錄(`[paths] records`) | 每次模型呼叫一筆(D-020);agent 的對話是真實使用,是階段 2 的原料(D-045)。`--no-record` 不寫 |
| 軌跡 | `<data_dir>/runs/<開始時間>_<run id>.jsonl` | `start`、每一步一個 `step`(完整回應、工具參數與結果)、`finish`(結果、用量、費用) |

兩者都有完整內容(包括 `read_file` 讀到的檔案),目錄 700、檔案 600,只存本機(D-017)。

## 當 library 用

```rust
use std::sync::Arc;
use agent::{Agent, AgentConfig, Toolbox, ErasedTool, Outcome};
use agent::tools::{Calculator, CurrentTime};

let tools: Vec<Box<dyn ErasedTool>> = vec![Box::new(Calculator), Box::new(CurrentTime)];
// provider:任何 `LlmProvider`,agent 只借用它(`&dyn LlmProvider`)。要套哪些 decorator(Record、Retry、Alias)由你決定。
let agent = Agent::new(&*provider, Arc::new(Toolbox::new(tools)?), AgentConfig::new("default"))?;

// 一次跑完:
let finished = agent.run("1234 乘 5678 是多少?").await;
// 或一步一步拉(CLI 這樣印進度、寫軌跡):
let mut run = agent.start("...");
while let Some(step) = run.next().await { /* step.tool_calls … */ }
let finished = run.finish();
```

寫新工具:參數寫成 struct(`Deserialize` + `JsonSchema`),實作 `Tool`。送給模型的 JSON Schema 由同一個 struct 產生,
解析成功就等於驗證通過(D-043)。欄位上的 `///` 會變成 schema 的說明、送給模型看。

```rust
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct WeatherArgs {
    /// 城市名稱,例如 "台北"。
    city: String,
}

struct Weather;

impl agent::Tool for Weather {
    type Args = WeatherArgs;
    fn name(&self) -> &str { "weather" }
    fn description(&self) -> &str { "查詢某個城市現在的天氣。" }
    async fn call(&self, args: WeatherArgs, _ctx: &agent::ToolContext) -> Result<String, agent::ToolError> {
        Ok(format!("{}:晴", args.city))
    }
}
```

## 原始碼導覽

| 檔案 | 內容 |
|---|---|
| `src/tool.rs` | `Tool`(有型別)、`ErasedTool`(抹除型別,MCP 工具之後實作它)、`Toolbox`(建立時檢查名稱、說明、schema) |
| `src/tools/` | 內建工具:`calculator.rs`、`clock.rs`、`files.rs` |
| `src/run.rs` | agent loop:`Agent`、`Run`(一步一步拉)、`Step`、`Outcome` |
| `src/limits.rs` | 護欄的數值 |
| `src/config.rs` | `agent.toml` 的形狀與檢查、費用計算 |
| `src/trajectory.rs` | 軌跡的事件與寫檔(700 / 600) |
| `src/prompt.rs` | 預設的 system prompt |
| `src/main.rs` | CLI(composition root:唯一讀設定檔、環境變數的地方) |
| `tests/e2e.rs` | 真的執行 agent binary,上游是 wiremock(多輪工具呼叫、護欄、紀錄、軌跡) |
