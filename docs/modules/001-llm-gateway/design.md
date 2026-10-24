# 001 LLM Gateway — 設計(第一部分:`crates/llm`)

> 狀態:**已定案(2026-10-24)**。§1~§3 整理 D-006~D-011;§4~§9 經使用者審核通過。

## 1. 要解決什麼

所有上層(Agent、Eval、RAG、應用)透過同一個介面呼叫 LLM,不直接碰任何 provider。
換 provider、計量、重試、快取都在這一層做。本文件只涵蓋**核心 library `crates/llm`**;
對外 HTTP server `crates/gateway` 另寫一份。

**不在這一部分做**:重試 middleware、計量、快取、OpenAI adapter、HTTP server、工具執行。

## 2. 結構(D-006)

```
Cargo.toml                 workspace
crates/llm/
  src/
    lib.rs                 #![forbid(unsafe_code)];只 re-export 公開型別
    types.rs               Request / Message / Content / Role / ToolDef / Usage
    stop.rs                StopReason(unified + raw)
    event.rs               StreamEvent
    error.rs               LlmError + 可重試分類
    provider.rs            trait LlmProvider
    accumulate.rs          事件流 → Response(純邏輯)
    sse.rs                 SSE 解析器(純邏輯、跟 provider 無關)
    anthropic/
      wire.rs              Anthropic 的 JSON 形狀(serde),只在這個模組內可見
      translate.rs         wire ⇄ 中立型別(純邏輯)
      client.rs            reqwest 呼叫、組 stream
  examples/chat.rs         手動試玩:cargo run --example chat
  tests/                   整合測試(wiremock)
  benches/                 criterion
```

依賴方向只有一條:`anthropic/` → 核心型別。核心型別不知道任何 provider 存在。
Vendor 的 wire 型別是 `pub(crate)`,不外洩(沿用 Go MODEL-01)。

## 3. 核心介面(D-009、D-010)

```rust
pub trait LlmProvider: Send + Sync {
    /// 唯一必須實作的方法。
    fn stream(&self, req: Request) -> BoxStream<'static, Result<StreamEvent, LlmError>>;

    /// 預設方法:收集 stream。每個 provider 自動擁有。
    fn complete(&self, req: Request) -> BoxFuture<'_, Result<Response, LlmError>> { ... }
}

pub enum StreamEvent {
    MessageStart  { id: Option<String>, model: String },
    TextStart     { index: usize, meta: ProviderMetadata },
    TextDelta     { index: usize, text: String },
    TextEnd       { index: usize, meta: ProviderMetadata },
    ThinkingStart { index: usize, meta: ProviderMetadata },
    ThinkingDelta { index: usize, text: String },
    ThinkingEnd   { index: usize, meta: ProviderMetadata },   // thinking 簽章放這裡
    ToolCallStart { index: usize, id: String, name: String, meta: ProviderMetadata },
    ToolArgsDelta { index: usize, partial_json: String },
    ToolCallEnd   { index: usize, meta: ProviderMetadata },
    Finish        { stop: StopReason, usage: Usage, meta: ProviderMetadata },
}

pub struct StopReason { pub kind: StopKind, pub raw: String }
pub enum StopKind { EndTurn, MaxTokens, StopSequence, ToolUse, Refusal, PauseTurn, Unknown }
```

- 錯誤就是串流裡的一個 `Err` 項目,之後串流結束。Rust 的 `Result` 已經能表達
  Vercel 的 `error` 事件,不必另外做一個 variant。
- 沒看過的 stop reason 對應到 `Unknown`,`raw` 保留原值;要不要當成失敗由上層決定
  (Agent loop 會當成失敗,fail-closed)。
- `Accumulator` 把事件組回 `Response`,同時檢查協定:`MessageStart` 必須最先、只出現一次;
  delta / end 必須對應到已開啟、種類相同的區塊;`Finish` 時不能有未關閉的區塊;
  `Finish` 之後不能再有事件;內容總量超過上限就拒絕。違反任何一條都回 `Protocol`。
- 事件上的 `meta` 一律保留:`Content` 的每個 variant、以及 `Response`,都帶 `meta`
  (實作第 4 步時補上,原本只有 `Thinking` 有)。

## 4. 提案:Request / Message 型別

```rust
pub struct Request {
    pub model: String,
    pub system: Option<String>,          // system 不是一種 Role(沿用 Go 的教訓)
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub provider_options: ProviderMetadata,   // effort、thinking 顯示方式等各家專屬設定
}
pub enum Role { User, Assistant }
pub struct Message { pub role: Role, pub content: Vec<Content> }
pub enum Content {
    Text       { text: String },
    Thinking   { text: String, meta: ProviderMetadata },
    ToolCall   { id: String, name: String, args_json: String },  // 原始 JSON,不可信,交給工具層解析
    ToolResult { call_id: String, content: String, is_error: bool },
}
pub struct ProviderMetadata(BTreeMap<String, serde_json::Value>);  // 以 provider 名稱為 key
```

- 刻意**沒有** `temperature` / `top_p` / `top_k`:Claude API 從 `2026-10-21` 起一律拒收,
  而且沒有替代參數;預設模型 Sonnet 5.5 本來就只接受預設值。輸出要穩定改用 prompt、
  structured outputs(JSON schema)、effort。其他還支援取樣參數的 provider(OpenAI、
  自家 vLLM)走 `provider_options`;某家不支援卻收到 → 回 `InvalidRequest`,**不默默丟掉**。
- 各家專屬的設定放進 `provider_options`(Vercel 的 `providerOptions` 模式),
  這樣核心型別不會被某一家污染。

## 5. 提案:錯誤與可重試分類

```rust
pub enum LlmError {
    Http { status: u16, retry_after: Option<Duration>, body: String },  // body 截斷到 4 KiB
    Transport(String),              // 連線/DNS/TLS;不外洩 reqwest 型別
    Timeout,
    Protocol(String),               // provider 送了壞掉的 SSE/JSON
    Interrupted { partial: Box<Response> },  // 已經有輸出之後才斷線
    InvalidRequest(String),         // 送出前的自我檢查失敗
}
impl LlmError { pub fn is_retryable(&self) -> bool }
```

- 不加 `#[non_exhaustive]`:專案不發布(D-013),讓 workspace 內的 `match` 保持窮舉檢查。
- `Display` 只印狀態碼,不印 body(body 可能夾帶使用者輸入)。
- `impl From<serde_json::Error>` → `Protocol`,讓 `?` 能直接轉換。

| 可重試 | 不可重試 |
|---|---|
| 408、409、429、500~599、529(overloaded)、`Transport`、`Timeout` | 400、401、403、404、413、`Protocol`、`InvalidRequest`、`Interrupted` |

## 6. 提案:串流中途失敗的語意

- adapter **一次請求只打一次**,不自己重試。重試是之後的 middleware 的事(單一職責)。
- 還沒送出任何內容事件就失敗 → 回傳一般錯誤,呼叫者可以安全重試。
- 已經送出內容才失敗 → `complete()` 回傳 `Interrupted { partial }`,不可以偷偷重試
  (內容已經給使用者看過了)。

## 7. 提案:安全

- API key 用 `secrecy::SecretString`:`Debug` 印出 `[REDACTED]`,不進 log。
- Library **不自己讀環境變數**,constructor 明確傳入 key;讀 env 是 `examples/` 或之後 binary 的事。
- 上限(超過就回 `Protocol` 錯誤,防止記憶體被灌爆):
  單一 SSE 事件 1 MiB、單次回應總量 32 MiB、錯誤 body 截斷 4 KiB。
- 解析路徑上不准出現 `unwrap()` / 索引 panic;用 `clippy::unwrap_used` 擋。
- **Log 與個資**(四層,越前面越可靠):
  1. **預設不記內容**:log 只記 metadata(model、token 數、耗時、request id、stop reason);
     prompt/回覆內容要另外開啟,寫到獨立的 sink。
  2. **型別遮罩**:`Request`、`Message` 手寫 `Debug`,只印數量與長度;敏感值包成
     `Sensitive<T>`,`Debug`/`Display` 一律印 `[REDACTED]`,要取值必須明確呼叫 `.expose()`。
     HTTP header 的 key 標記 `set_sensitive(true)`。
  3. **假名化**:需要追蹤「同一個使用者」時,記 HMAC(金鑰, user_id),不記原值,也不用沒加鹽的 hash。
  4. **樣式掃描**(最後一道防線):tracing layer 用正則遮蔽 email、電話、信用卡、身分證字號;
     只當補漏,不當主要手段。
- **Canary 測試**:用一把假 key `sk-ant-CANARY-…` 跑完整流程,斷言它沒出現在任何
  log、錯誤訊息、`Debug` 輸出裡。
- TLS 用 `rustls`,不用 OpenSSL。
- SSE 解析(第 5 步):照 WHATWG 規格;不合法的 UTF-8 回 `Protocol`(不做替換字元);
  忽略 `id` 與 `retry`(LLM API 不支援斷點續傳);單一事件上限同時計入「還沒換行的那一行」與已累積的 data。

## 8. 提案:效能與執行期

- 整個 provider 共用一個 `reqwest::Client`,連線池重複使用。
- 回傳 `BoxStream` 而不是 `impl Stream`:Gateway 要在執行期用 `Arc<dyn LlmProvider>` 選 provider,
  `dyn` 需要裝箱。每次請求多一次 heap 配置,跟網路延遲相比可以忽略(benchmark 驗證)。
- **取消 = drop**:丟掉 stream 就會中止 HTTP 請求。Rust 不需要 Go 那種 `context.Context`。
- Anthropic adapter 預設帶 `fallbacks: "default"`(beta `server-side-fallback-2026-07-01`),
  遇到 refusal 時由 API 端自動改用其他模型;可以透過 `provider_options` 關掉。
- Anthropic 翻譯(第 6 步):`provider_options["anthropic"]` 的欄位攤平進請求 body,但不准覆蓋核心欄位
  (`model`、`messages` 等,違反回 `InvalidRequest`);值為 `null` 表示拿掉該欄位。
  沒有 Anthropic 簽章的 thinking 不送;工具參數必須是 JSON 物件(用 `RawValue` 驗證、不重新編碼)。
  收回時:未知的事件類型忽略(Anthropic 的版本政策),未知的內容區塊與 delta 類型回 `Protocol`;
  串流中的 `error` 事件依類型對應到 HTTP 狀態碼,沿用同一套可重試規則;refusal 的 `stop_details` 放進 `Finish` 的 meta。

## 9. 提案:實作順序(一個 commit 教一個概念)

| # | 內容 | 主要 Rust 概念 | 測試 |
|---|---|---|---|
| 1 | workspace、`crates/llm` 骨架、CI | Cargo workspace、module、lint 設定 | CI 綠燈 |
| 2 | `types.rs`、`stop.rs` | struct、enum、derive、serde | 單元 |
| 3 | `error.rs` | 帶資料的 enum、`thiserror`、`Result`、`?` | 單元 |
| 4 | `event.rs` + `accumulate.rs` | match、ownership、`Vec`、狀態機 | 單元 |
| 5 | `sse.rs` | `&[u8]` / `&str`、borrowing、增量解析 | 單元 + proptest |
| 6 | `anthropic/wire.rs` + `translate.rs` | serde tagged enum、`pub(crate)` | 單元 + insta |
| 7 | `provider.rs` + `anthropic/client.rs` | async、`reqwest`、`BoxStream`、`Send + Sync`、`secrecy` | 整合(wiremock) |
| 8 | `examples/chat.rs` + live test | binary、env、`#[ignore]` | live(手動) |
| 9 | SSE benchmark | criterion | bench |

前 6 步都是同步的純邏輯,async 到第 7 步才出現。
