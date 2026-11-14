# 003 Agent Runtime — 設計(第一部分:agent loop 與內建工具)

> 狀態:**已定案(2026-11-14)**。四個分岔由使用者選定(D-041 ~ D-045,記在 ROADMAP 決策表);
> §10 的五個小決定都照建議。

## 1. 要解決什麼

讓模型能「做事」:模型說要用哪個工具、參數是什麼,程式去執行,把結果交回模型,直到模型給出答案。

- **階段 1**:個人助理的核心(005 把它接上介面)。用 agent 的每一次模型呼叫都經過 Record,成為階段 2 的原料。
- **階段 3**:RL 要的「能驗證的任務」很多是多步驟的(讀檔、算、跑測試),需要一個可靠的 loop。
- **002**:eval 加上 `Input::Agent`(D-029 預留的位置),用數字回答「給工具之後,答得比較好嗎?」「Haiku 當 agent 夠不夠用?」。

**003 分三部分(D-041)**,每部分都能單獨跑起來:

| 部分 | 內容 |
|---|---|
| **一(這份文件)** | agent loop、工具介面、沒有副作用的內建工具、護欄、軌跡、CLI、eval 整合 |
| 二 | MCP client:把 MCP server 的工具接到同一個工具介面 |
| 三 | 沙箱:會寫檔、跑程式的工具;順便做 D-040 的 `compiles` 檢查 |

## 2. 決策摘要

| # | 決策 | 理由 |
|---|---|---|
| D-041 | 003 分三部分:① loop + 工具介面 + 安全的內建工具 ② MCP client ③ 沙箱 | 每部分都能單獨看到成果;MCP 和沙箱各自是大題目,混在一起回饋太慢 |
| D-042 | loop 是單純的迴圈(呼叫模型 → 執行工具 → 結果接回去);每一步產生一筆事件(軌跡);引擎自己強制終止條件;停止原因不認得、或前後矛盾,一律當失敗 | 簡單好讀;軌跡可以除錯、給 eval 打分、之後做接續。狀態機要等真的有「人工核准」的工具再說,現在做是預先抽象 |
| D-043 | 內建工具的參數寫成 Rust struct:serde 解析成功就等於驗證通過;送給模型的 JSON Schema 由同一個 struct 產生(`schemars`)。工具介面另有一層「型別抹除」的版本,第二部分的 MCP 工具直接實作它 | 參數是不可信輸入;schema 和解析用同一個型別,兩邊不會不一致。MCP 工具的 schema 是對方給的,只能在執行期驗證 |
| D-044 | 第一批工具都沒有副作用:`calculator`(自己寫算式 parser)、`current_time`、`read_file`(只讀、限定在指定目錄內) | 第一部分不需要沙箱;`read_file` 擋「跳出目錄」本身就是安全題材 |
| D-045 | `crates/agent` 是 library,直接用 `llm`(D-006);**CLI 套 Record**(agent 的對話是真實使用),eval 跑 agent 時**不套**(D-033);軌跡由 binary 寫進私有目錄 | Record 要不要套,由組裝的地方(composition root)決定,library 不知道;library 不寫檔(D-016) |

## 3. 結構

```
crates/agent/
  src/
    lib.rs
    tool.rs          Tool trait(有型別)、ErasedTool(抹除型別)、Toolbox(依名稱找工具)
    tools/
      calculator.rs  自己寫的算式 parser
      clock.rs       current_time
      files.rs       read_file、list_dir(§10-1),共用「路徑不能跳出目錄」的檢查
    limits.rs        AgentLimits(護欄的數值)
    run.rs           agent loop:Agent、Run、Step、Outcome
    main.rs          CLI(composition root,唯一讀設定檔與環境變數的地方)
  tests/e2e.rs       真的執行 agent binary,上游是 wiremock(多輪:先 tool_use、再 end_turn)
agent.example.toml   進 git;agent.toml 不進(§10-4)
```

依賴方向:`agent` → `llm`、`agent` → `config-loader`;`eval` → `agent`(第 6 步)。`llm` 不知道 agent 存在。

**Harness² 的「方法 / 執行器 / benchmark 分開」**:`AgentConfig`(system prompt、工具組合、護欄)是方法,
`Run` 是執行器,eval 的資料集是 benchmark。三者可以各自替換。

## 4. 工具介面(D-043)

兩層。寫內建工具的人只看第一層;loop 只看第二層。

```rust
// 第一層:有型別。參數 struct 同時 derive Deserialize 和 JsonSchema。
pub trait Tool: Send + Sync {
    type Args: DeserializeOwned + JsonSchema + Send;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn call(&self, args: Self::Args, ctx: &ToolContext) -> impl Future<Output = Result<String, ToolError>> + Send;
}

// 第二層:抹除型別,可以放進 Vec<Box<dyn ErasedTool>>。參數是模型給的原始 JSON 字串。
pub trait ErasedTool: Send + Sync {
    fn def(&self) -> ToolDef;
    fn call_json<'a>(&'a self, args_json: &'a str, ctx: &'a ToolContext) -> BoxFuture<'a, Result<String, ToolError>>;
}
// 所有 Tool 自動實作 ErasedTool(blanket impl):先用 serde 解析 args_json,失敗就回 ToolError::InvalidArgs。
```

- 為什麼要兩層:有關聯型別(`type Args`)、回傳 `impl Future` 的 trait 不能當 `dyn` 用
  (Java 對照:`List<Tool<?>>` 可以裝不同參數型別的工具,因為泛型在執行期被抹除;Rust 要自己做這一層)。
- 參數 struct 加 `#[serde(deny_unknown_fields)]`:模型多給欄位也算錯。schemars 會產生 `additionalProperties: false`。
- 產生出來的 schema 用 insta snapshot 鎖住(跟 001 鎖住送出的 request JSON 同理)。
- **`Toolbox`**:建立時檢查(fail-loud):名稱符合 `^[a-zA-Z0-9_-]{1,64}$`(Anthropic 的規定)、不重複、說明不是空的。
  產生 `Vec<ToolDef>` 給 `Request.tools`;依名稱找工具執行。
- **`ToolContext`**:`run_id`、`call_id`(idempotency key:之後有副作用的工具,接續時靠它避免做兩次)、
  時鐘(測試與 eval 可以固定時間)。
- **錯誤一律交回模型**:參數錯、工具失敗、找不到工具,都變成 `is_error: true` 的 `ToolResult`,讓模型自己修正。
  錯誤訊息只寫模型需要知道的(例如 `read_file` 只寫相對路徑,不洩漏目錄外的絕對路徑)。

### 實作時定下的(第 1 步,`tool.rs`、`tools/clock.rs`)

- `ErasedTool::def()` 回傳 `ToolDef`(不是參考):blanket impl 沒有地方存算好的定義;`Toolbox` 建立時算一次、存起來。
  抹除型別的方法叫 `call_json`,不叫 `call`:同一個型別同時實作兩個 trait,兩個都叫 `call` 會讓呼叫端寫 `t.call(..)` 時有歧義。
- **參數必須是 JSON 物件**:serde derive 出來的 struct 也接受 JSON 陣列(照欄位順序填,`[1, 2]` 變成 `a = 1, b = 2`),
  比 schema 寬鬆。所以先解析成 `Value`、確認是物件,再轉成參數型別。這是測試抓到的(第 1 步的空殼測試)。
- 產生 schema:draft 2020-12、不要 `$schema`、拿掉 `title`(Rust 的型別名稱)、巢狀型別直接展開(不用 `$defs` + `$ref`)。
  參數 struct 欄位上的 `///` 會變成 schema 的 description、送給模型,所以寫給模型;給讀程式的人的說明用 `//`。
- `Toolbox` 的錯誤訊息裡,工具名稱用 `{:?}` 印(跳脫換行等字元):第二部分的名稱來自 MCP server,不可信。
  也檢查 schema 的 `type` 是 `object`(Anthropic 的規定;參數是 `i64` 之類的工具會被拒絕)。
- `current_time` 回傳 `2026-11-14T10:30:00+08:00[Asia/Taipei], Saturday`;星期幾用當地日期算。時區名稱先檢查形狀
  (最多 64 字元、以 `/` 分段、每段非空、只有英數字與 `_` `-` `+`)再查 jiff。jiff 本來就只比對它掃描 zoneinfo 得到的名稱清單,
  讀不到目錄外的檔案;自己再檢查一次是不依賴第三方的內部行為,錯誤訊息也更清楚。

## 5. Agent loop(D-042)

```rust
let agent = Agent::new(provider, toolbox, config);   // provider 是 Arc<dyn LlmProvider>,由 CLI 或 eval 決定套哪些 decorator
let mut run = agent.start(task);                       // 還沒呼叫模型
while let Some(step) = run.next().await { /* 印出、寫軌跡 */ }   // 一步 = 一次模型呼叫 + 這一輪的工具
let finished = run.finish();                           // Outcome、全部的 Step、用量合計
// 不需要一步一步看的話:agent.run(task).await
```

狀態(對話、計數器、截止時間)和護欄都在 `Run` 裡面,呼叫者只能「拉下一步」,繞不過護欄(Java 對照:像 `Iterator`)。
第一部分用 `complete()`(整輪收完再處理),不做逐字串流;CLI 每一步印一次。

**每一輪依模型的停止原因決定(`match` 列出全部,compiler 檢查沒有漏掉)**:

| 停止原因 | 處理 |
|---|---|
| `EndTurn` | 結束,回答 = 這一輪的文字 |
| `ToolUse` | 執行這一輪的全部工具呼叫,結果放進**同一則** user 訊息,繼續下一輪 |
| `MaxTokens` | 失敗:回答被截斷,工具呼叫的參數可能不完整 |
| `Refusal` | 結束,結果是「拒答」 |
| `PauseTurn` | 失敗:只有伺服器端工具會出現,我們沒開 |
| `StopSequence` | 失敗:我們沒設停止字串,出現就是不對勁 |
| `Unknown` | 失敗(fail-closed) |

前後矛盾也當失敗,不猜:停止原因是 `ToolUse` 卻沒有工具呼叫、有工具呼叫但停止原因不是 `ToolUse`、同一次執行裡出現重複的工具呼叫 id。

**工具執行**:

- 同一輪的多個呼叫並行執行(上限 `max_parallel`),結果照原本的順序排回去(`buffered`,不是 `buffer_unordered`)。
- 每次呼叫有時間上限;超過就回 `is_error`。
- 參數字串先檢查長度(上限 64 KiB)再解析;結果超過上限(32 KiB)就截斷,加上「已截斷 N bytes」的標記(切在 UTF-8 字元邊界)。
- thinking 區塊(含簽章)跟著 assistant 訊息原封不動接回去:Anthropic 規定用工具時要這樣做。整則 `content` 直接放回去就滿足。

**護欄(引擎自己的,呼叫者和模型都改不了;數值見 §10-3)**:

| 護欄 | 觸發時 |
|---|---|
| 最多幾輪(模型呼叫次數) | 停止,`LimitReached(Turns)` |
| 最多幾次工具呼叫(整次執行) | 這一輪會超過就整輪不執行、停止,`LimitReached(ToolCalls)`(要嘛整輪執行、要嘛停,不做一半) |
| token 合計(每輪的 input + output 加總) | 停止,`LimitReached(Tokens)`。每輪都重送整段對話,input 會越滾越大,這條是成本的硬上限 |
| 總時間 | 停止,`LimitReached(Deadline)` |

觸發護欄就停,不再給模型「最後一次回答」的機會:護欄要簡單、可預期。

```rust
pub enum Outcome {
    Answered { text: String },
    Refused,
    LimitReached(Limit),
    Failed(AgentError),   // 模型呼叫失敗(Retry 之後)、停止原因不對、前後矛盾
}
```

## 6. 軌跡

每一步是一個可序列化的 `Step`,內容完整(請求的摘要、完整回應、每個工具呼叫的參數與結果、耗時、用量)。

- library 只產生 `Step`;CLI 寫成 JSONL:`<資料目錄>/agent/runs/<時間>_<run id>.jsonl`(目錄 700、檔案 600,規則同 eval)。
  `read_file` 讀到的內容會在軌跡和呼叫紀錄裡,所以一樣只存本機(D-017)。
- run id 用 UUID v7(開頭是時間)。
- 軌跡和呼叫紀錄會有重複的內容(每次模型呼叫兩邊都有)。這次接受重複;
  「呼叫紀錄記上它屬於哪次 agent 執行」要改 `CallRecord` 的格式,等 006 分析紀錄時再決定(記進待辦)。

## 7. 內建工具(D-044)

| 工具 | 參數 | 重點 |
|---|---|---|
| `calculator` | `expression` | 自己寫 lexer + Pratt parser:`+ - * / % ^`、括號、負號、`sqrt` `abs` 等幾個函式、`pi` `e`。不用任何 eval。**算式長度與括號深度設上限**:遞迴太深會 stack overflow,那是整個程式直接中止、不是可以處理的錯誤。結果不是有限的數(除以 0、溢位)就回錯誤。proptest:任意輸入都不 panic |
| `current_time` | `timezone`(可選,IANA 名稱) | 回 RFC 3339 時間與星期幾。時鐘從 `ToolContext` 拿,測試和 eval 可以固定 |
| `read_file` | `path`、`offset`、`limit`(行) | 見下 |
| `list_dir` | `path` | §10-1:不知道有哪些檔案,`read_file` 很難用 |

**`calculator` 實作時定下的(第 2 步,`tools/calculator.rs`)**:

- 三段:lexer(字串 → token)→ Pratt parser(token → 語法樹 `Expr`)→ eval(語法樹 → `f64`)。
- 優先順序:`+ -` < `* / %` < 負號 < `^`;`^` 由右往左(`2 ^ 3 ^ 2 = 512`),`-2 ^ 2 = -4`(跟數學課本一樣)。
  binding power 刻意用一奇一偶(1/2、3/4、8/7),左右兩邊永遠不會相等,所以比較時 `<` 和 `<=` 等價。
- 不支援省略乘號(`2(3)`)、`%` 是餘數不是百分比:都回錯誤,不猜。錯誤訊息附「第幾個字元」(不是第幾個 byte)。
- 數字:`12`、`1.5`、`.5`、`1e3`、`2E-1`;`e` 後面接數字才算指數,否則是常數 `e`(要往後看兩個字元,複製 iterator 來看)。
- 上限:算式 1024 bytes、巢狀 64 層。長度上限讓巢狀最多約 500 層,本來不會 stack overflow;深度上限是另一道防線,
  測試直接對 parser 餵 10 萬層括號。拿掉深度檢查的 mutant 讓整個測試程式 stack overflow、SIGABRT 中止。
- lexer 每一輪**先拿走一個字元**再處理,迴圈一定往前走。原本「先 peek、各分支自己拿走」的寫法,CI 上的 cargo-mutants
  把 `take_while` 換成回傳空字串之後,lexer 一直重讀同一個字元、一直塞 token,幾秒內要幾 GB 記憶體,整台 runner 被關掉。
- 每一步都檢查結果是有限的數(`1 / (10 ^ 400)` 不能變成 0);除以 0 是 `Failed`(算式沒錯、算不出來),其餘是 `InvalidArgs`。
- 排版:`1e-6 <= |x| < 1e16` 用一般寫法(2^53 以內的整數印出每一位),其餘用科學記號;沒有 `-0`。

選 `calculator` 不只是範例:模型算大數字常錯,這是 eval 量得出「工具有沒有幫助」的地方。

**`read_file` / `list_dir` 的安全規則**(工具建立時給一個根目錄;CLI 沒給 `--root` 就不註冊這兩個工具):

- 只收相對路徑;接到根目錄後 `canonicalize`(會解開 symlink),結果必須仍在根目錄底下。擋 `../` 和指向外面的 symlink。
- 拒絕任何一段以 `.` 開頭的路徑(`.env`、`.git/`),§10-5。
- 只讀一般檔案;大小上限(256 KiB);不是 UTF-8 就回「不是文字檔」。
- 已知的剩餘風險:`canonicalize` 和開檔之間,有人把檔案換成 symlink(TOCTOU)。第一部分沒有任何工具能寫檔,
  所以只有本機其他程式做得到;第三部分的沙箱再處理。Linux 的 `openat2(RESOLVE_BENEATH)`(`cap-std` crate)能根本解決,
  但不在這部分。
- 測試:用 tempfile 建目錄,真的建 symlink(`#[cfg(unix)]`),驗證跳出目錄的每一種寫法都被擋。

**工具結果也是不可信輸入**:檔案內容可能寫著「忽略之前的指示」。第一部分的工具都不能寫入、不能連網,
被騙最多是答錯,沒有把資料送出去的管道。有網路工具之後(之後的部分)要重新評估。

## 8. CLI 與設定

```
agent run "問題" [--model default] [--root <目錄>] [--system <檔案>] [--max-turns 10] [--no-record]
```

- 設定檔 `agent.toml`(§10-4):別名、`[anthropic] base_url`、`[record] dir`(預設跟 gateway 一樣,飛輪讀同一個地方)、
  護欄預設值、價格(有的話顯示費用,沒有就顯示「未知」)。`deny_unknown_fields`,錯字拒絕執行。
- API key 同 eval:`ANTHROPIC_API_KEY`(`.env`)。
- provider 鏈:`Record [ Retry [ Alias [ AnthropicClient ] ] ]`(D-020 的順序);`--no-record` 拿掉 Record(試驗用)。
- 預設的 system prompt 寫在程式裡(繁體中文、說明有哪些工具、什麼時候該用),可以用 `--system` 換。這就是 Harness² 的
  「system prompt」和「tool guidance」兩項;其餘六項(memory、subagents、skills…)不在第一部分。

## 9. eval 整合

- `Input::Agent(AgentInput)`:`system`、`prompt`、`tools`(內建工具的名稱)、`max_tokens`、可選的 `now`(固定時鐘)、
  可選的 `root`(檔案工具的根目錄,放 `evals/fixtures/<資料集>/`,**不能**是 `evals/` 本身:裡面有評分標準)。
- `Output::Agent`:最後的回答 + 全部的 `Step`。確定性檢查和評審看最後的回答;之後可以加看軌跡的檢查(例如「有沒有呼叫 calculator」)。
- 加 variant 之後,compiler 會列出 eval 裡每一個還沒處理它的 `match`(D-029 當初的承諾,這次驗證)。
- 新資料集 `evals/agent-basics.jsonl`(約 15~20 題,以確定性檢查為主):需要精確計算的題目、需要知道今天日期的題目(固定時鐘)、
  要讀 fixture 檔案才答得出來的題目。
- 真的跑一次:Haiku 對 Sonnet(都給工具),以及 Haiku 給工具對不給工具(同樣的題目用 `Input::Chat`)。

## 10. 已決定的小事(2026-11-14,使用者確認,都照建議)

1. **加 `list_dir`**(建議加):只讀、跟 `read_file` 共用路徑檢查;沒有它,模型只能猜檔名。
2. **eval 整合放在第一部分最後**(建議放):第一部分結束時就有數字,不只是「跑得起來」。不放的話,第一部分在第 5 步結束。
3. **護欄預設值**(建議):最多 10 輪、整次執行最多 30 次工具呼叫、同一輪並行 4 個、token 合計 200,000、總時間 5 分鐘、
   每次工具呼叫 30 秒、工具結果 32 KiB、參數 64 KiB、`read_file` 檔案 256 KiB。都可以由 `agent.toml` 或參數改,但不能關掉。
4. **設定檔各自一份 `agent.toml`**(建議):跟 gateway、eval 的做法一致(共用等 011)。另一個做法是讀 `eval.toml`,
   但兩個工具的設定混在一起,改一邊會影響另一邊。
5. **`read_file` 拒絕以 `.` 開頭的路徑**(建議拒絕):`.env`、`.git/` 最常放秘密;真的需要再加白名單。

## 11. 實作順序(一個 commit 教一個概念)

| # | 內容 | 主要 Rust 概念 |
|---|---|---|
| 1 | crate 骨架、`Tool` / `ErasedTool` / `Toolbox`、`current_time` **(完成)** | 關聯型別、trait 回傳 `impl Future`、blanket impl 做型別抹除、schemars derive、insta 鎖 schema |
| 2 | `calculator` **(完成)** | `Peekable<Chars>` 寫 lexer、Pratt parser(binding power)、遞迴深度上限、proptest |
| 3 | `read_file`、`list_dir` | `Path` / `PathBuf` / `Component`、`canonicalize`、symlink 測試、`#[cfg(unix)]` |
| 4 | agent loop:停止原因、護欄、並行執行、結果截斷(假的 provider 照劇本回應) | 窮舉的 `match`、`buffered`、`tokio::time::timeout`、UTF-8 邊界 |
| 5 | 軌跡、CLI、`agent.toml`、Record、E2E(wiremock 多輪)、crate README、`examples/` | serde tagged enum、clap、composition root |
| 6 | eval `Input::Agent`、`agent-basics` 資料集、E2E | 加 enum variant 跟著 compiler 錯誤走 |
| 7 | 真的跑(§9),結果寫進 README;retro | — |

## 12. 不在第一部分

- MCP(第二部分)。沙箱、寫檔、跑程式、`compiles` 檢查(第三部分)。
- `web_fetch`:要處理 SSRF、回應大小、eval 時 agent 上網找到公開的評分標準(Harness² 看過的作弊)。
- 逐字串流到 CLI、人工核准、當機後接續(軌跡已經留了接續需要的資料)、memory、subagents、skills。
- 重複呼叫偵測(同樣的工具與參數一直呼叫):是猜測規則,可能誤擋合理的輪詢;有實際案例再加。

## 13. 費用估算(第 7 步,粗估;跑之前用 `--dry-run` 估最壞情況並設 `--budget`)

| 項目 | 估計 |
|---|---|
| `agent-basics` 約 18 題 × 每題 2~4 次模型呼叫:Haiku、Sonnet 各一次 | 約 $0.3 |
| Haiku 不給工具(對照)+ A/A | 約 $0.05 |
| 評審(只有少數題目有評分標準) | 約 $0.1 |
| **合計** | **約 $0.5** |
