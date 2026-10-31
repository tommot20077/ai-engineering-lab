# 001 LLM Gateway — 設計(第二部分:decorator + `crates/gateway`)

> 狀態:**已定案(2026-10-31)**。整理 D-017~D-025,逐項討論過;本文件補上結構、介面與實作順序。
> 第一部分見 `design.md`(§1~§9),本文件從 §10 接著編號。

## 10. 要解決什麼

第一部分做出「怎麼呼叫一次 LLM」。第二部分讓**每一次呼叫**都經過同一組規則,並開放給 Rust 以外的程式:

- **Alias**:請求寫別名(`cheap`),換成真正的 model ID;別名表就是成本與換模型的控制點。
- **Retry**:輸出前的暫時性失敗自動重試。
- **Record**:每次呼叫留下一筆完整紀錄,是階段 2 資料飛輪的原料。
- **`crates/gateway`**:HTTP 外殼,讓 Python、Java、IDE 也能呼叫。

**不在這一部分做**:OpenAI 相容端點(D-011,之後)、多使用者、執行中替換別名表(011)、快取、計費報表。

## 11. 結構

```
crates/llm/src/
  provider.rs              + blanket impl:Arc<T: LlmProvider + ?Sized> 也是 provider
  middleware/
    mod.rs                 LlmProviderExt(with_alias / with_retry / with_record)
    alias.rs               Alias<P>
    retry.rs               Retry<P> + RetryPolicy
    record/
      mod.rs               Record<P> + RecordSink trait
      model.rs             CallRecord(紀錄的 JSON 形狀)
      jsonl.rs             JsonlWriter(背景執行緒寫檔)
crates/config-loader/       給 binary 用的共用設定工具(D-025);library 不依賴它
  src/lib.rs               路徑展開、預設資料目錄、讀 TOML、讀秘密環境變數
crates/gateway/
  src/
    lib.rs                 build_app(state) -> Router(給 E2E 測試用)
    main.rs                composition root:讀 .env + gateway.toml、組裝、啟動
    config.rs              設定檔型別 + 啟動檢查
    api.rs                 handlers
    error.rs               ApiError → HTTP 回應
    guard.rs               token、Host 檢查
  tests/e2e.rs             真 server(隨機 port)+ wiremock 當假 Claude
gateway.example.toml       進 git;gateway.toml 不進(D-024)
```

依賴方向:`gateway` → `llm`、`gateway` → `config-loader`。`llm` 不知道 HTTP server 存在,也不讀任何檔案或環境變數(D-016);
`JsonlWriter` 會**寫**檔,但目錄由 constructor 傳入。

## 12. Decorator 機制(D-018、D-019)

```rust
// 模具:測試時填具體型別,gateway 填 Arc<dyn LlmProvider>
pub struct Alias<P> { inner: P, table: AliasTable }
impl<P: LlmProvider> LlmProvider for Alias<P> { ... }

// 讓「盒子」也算 provider。stream 與 complete 都要轉發:
// AnthropicClient 覆寫了 complete(用自己的 Limits),只轉發 stream 會悄悄改用預設上限。
impl<T: LlmProvider + ?Sized> LlmProvider for Arc<T> { ... }

// 組裝語法
pub trait LlmProviderExt: LlmProvider + Sized {
    fn with_alias(self, table: AliasTable) -> Alias<Self>;
    fn with_retry(self, policy: RetryPolicy) -> Retry<Self>;
    fn with_record(self, sink: Arc<dyn RecordSink>) -> Record<Self>;
}
impl<P: LlmProvider> LlmProviderExt for P {}
```

**規則:每個 decorator 都實作 `stream` 和 `complete`,各自交給裡面那一層的同名方法。**
只寫 `stream` 的話,`complete` 走 trait 預設方法,會跳過裡面覆寫的 `complete`(例如 client 自己的 `Limits`);
第 10 步的測試與 mutation 檢查專門盯這件事。

**包裝順序(D-020)**:`Record [ Retry [ Alias [ client ] ] ]`。

- Record 在最外層:一次呼叫一筆、看得到別名、量到使用者感受的總延遲。
- 需要在串流進行中再呼叫 `inner` 的 decorator(Retry)內部存 `Arc<P>`:`stream()` 立刻回傳,
  重試發生在之後讀取時,那時 `&self` 已經結束。

## 13. Alias

- `AliasTable`:別名 → model ID 的 map,加上 `strict: bool`。
- 嚴格模式(gateway 使用):不在表裡 → 回傳只有一個 `Err(InvalidRequest("unknown model alias ...; available: ..."))` 的串流,不碰網路。
- 寬鬆模式:不在表裡就原樣送出。由 constructor 決定,library 不替呼叫者決定(D-016)。
- `Alias` 不知道表從哪裡來;011 要在執行中替換時,換掉提供表的東西即可(D-024)。

## 14. Retry(D-022)

- 只在**第一個輸出事件之前**重試;第一個輸出前的事件(`MessageStart`、`TextStart` 等)先扣住,
  確定不重試才依原順序放行;整段都沒有輸出就在結束時放行。`is_output()` 從 `provider.rs` 改成 `pub(crate)` 共用。
- 是否可重試只看 `LlmError::is_retryable()`。
- `RetryPolicy::default()`:最多 3 次;第 n 次失敗後等 `random(0, min(max_delay, first_delay × 2^(n-1)))`
  (full jitter;first 1 秒、max 10 秒)。有 `retry-after` 就照它;**超過 max 就不等,直接回傳錯誤**。
- 等待用 `tokio::time::sleep`(不佔執行緒);測試用 tokio 的暫停時間,不真的等。
- 每次重試寫一筆 `tracing` 營運 log(第幾次、錯誤種類、等多久),不含內容(D-017)。

## 15. Record(D-020、D-021)

**介面**:

```rust
pub trait RecordSink: Send + Sync {
    /// 不可阻塞、不回傳錯誤:失敗由 sink 自己計數並寫 tracing ERROR(D-021 (3))。
    fn submit(&self, record: CallRecord);
}
```

`RecordSink` 是擴展點(儲存後端);目前實作 `JsonlWriter`,測試用記憶體版本。

**紀錄形狀**(`CallRecord`,序列化成一行 JSON):

| 欄位 | 內容 |
|---|---|
| `schema_version` | `1` |
| `id` | UUID v7(依時間排序) |
| `started_at` | RFC 3339、UTC |
| `requested_model` / `served_model` | 請求的別名 / `Response.model`(含伺服器端 fallback);失敗時 served 為 null |
| `request` | 完整 `Request`(含 system、tools、provider_options) |
| `response` | 完整 `Response`;`interrupted` / `cancelled` 時是部分回應;`error` 時為 null |
| `outcome` | `ok` / `error` / `interrupted` / `cancelled` |
| `error` | `{ kind, status?, message }`,沿用 `LlmError` 的 `Display`(不含上游 body) |
| `usage` | token 數 |
| `latency_ms` | `{ first_output, total }`;沒有輸出時 first_output 為 null |

**運作**:Record 邊抄邊傳,不扣事件;用 `Accumulator` 收一份副本。結束時依情況決定 `outcome`:
收到 `Finish` → ok;輸出前出錯 → error;輸出後出錯 → interrupted;**串流被丟掉**(使用者取消)→ cancelled,
用 `Drop` 送出。每筆只送一次。
裡面那一層吐出不合協定的事件時,Record 照樣轉送(它只是旁觀者),但這筆記成 error、不附回應。

**JsonlWriter**:

- `JsonlWriter::start(dir, capacity) -> JsonlWriter`:開一條**背景 OS 執行緒**,用有上限的 `std::sync::mpsc::sync_channel` 收紀錄。
  寫檔是阻塞 I/O,放在獨立執行緒,不佔 tokio 的工作執行緒。
- `submit` 用 `try_send`:佇列滿(預設 1000)或寫入失敗 → 丟棄、`dropped` 計數 +1(`AtomicU64`)、tracing ERROR。
- 檔名取自紀錄的 `started_at`(UTC 日期)`YYYY-MM-DD.jsonl`,所以換日可以測試、不必注入時鐘。
- 目錄建立時 `700`、檔案建立時 `600`(`DirBuilderExt` / `OpenOptionsExt`,建立當下就設定,不是事後 chmod)。
  只支援 Unix(macOS / Linux)。
- 一筆 = 先組好整行(含換行)再一次寫入;讀取端(007)跳過最後一行殘缺的資料,寫進 gateway README。
- 開檔時若最後一個字元不是換行(上次當機留下半行),先補一個換行再寫,避免下一筆完整的紀錄黏在半行後面一起壞掉。
- 換日與寫檔執行緒結束時各 `sync_data()` 一次:每筆都 sync 太慢;最多遺失斷電前最後幾秒。
- 只依日期切檔,不依大小切(每天約數 MB);保留期限與壓縮留到 006 一起設計。
- `shutdown(self)`:關閉佇列、寫完剩下的紀錄、等執行緒結束。

## 16. gateway:設定與啟動(D-024、D-025)

**`config-loader`**(所有讀設定檔的 binary 共用,現在只放 gateway 需要的):

| 函式 | 規則 |
|---|---|
| 路徑解析 | 只展開開頭的 `~/` → 家目錄;不展開 `$VAR`;**相對路徑以設定檔所在目錄為準**(同 Cargo),不以執行指令的目錄為準 |
| 預設資料目錄 | macOS `~/Library/Application Support/ai-engineering-lab/<程式名>`;Linux `~/.local/share/ai-engineering-lab/<程式名>` |
| 讀 TOML | 錯誤訊息帶檔名與行號;找不到檔案時附上提示 |
| 讀秘密環境變數 | 直接包成 `SecretString`;沒設定給出清楚錯誤 |

`main.rs` 是 composition root,依序:

1. `dotenvy` 讀 `.env`:`ANTHROPIC_API_KEY`、`GATEWAY_TOKEN`,都包 `SecretString`;缺任何一個就拒絕啟動。
2. 讀 `--config <path>`(預設 `./gateway.toml`);找不到 → 提示 `cp gateway.example.toml gateway.toml`。
3. 解析與檢查(`deny_unknown_fields`):別名表非空;retry 數值合理;`bind` 不是本機位址時必須 `allow_remote = true`
   並列出 `allowed_hosts`;紀錄目錄經 `config-loader` 解析、建立或檢查權限、確認可寫。任何一項不過就印原因並結束。
4. 組裝 `Record [ Retry [ Alias [ AnthropicClient ] ] ]`,型別是 `Arc<dyn LlmProvider>`;依 `record.enabled` 決定包不包 Record。
5. 印出 bind 位址與紀錄目錄,啟動 axum。

```toml
[server]
bind = "127.0.0.1:8787"
# allow_remote = false

[aliases]
default = "claude-sonnet-5-5"
cheap   = "claude-haiku-5-5"

[anthropic]                # 可省略;base_url 給 E2E 測試指向假 server
# base_url = "https://api.anthropic.com"

[record]                   # 可省略
enabled = true
# dir = "./records"      # 預設 default_data_dir("gateway")/records,例如 macOS 的
                         # ~/Library/Application Support/ai-engineering-lab/gateway/records

[retry]                    # 可省略 → RetryPolicy::default()
max_attempts = 3
first_delay_ms = 1000
max_delay_ms = 10000

[limits]                   # 可省略
max_body_bytes = 10485760
max_concurrent = 16
```

## 17. gateway:HTTP API(D-023)

| 端點 | 回應 |
|---|---|
| `POST /v1/chat` | body = `Request`,回完整 `Response`(JSON);總時間上限 10 分鐘 → 504 |
| `POST /v1/chat/stream` | body = `Request`,回 SSE:`event: <type>`、`data: <StreamEvent JSON>`,meta 原樣傳出 |
| `GET /v1/models` | 別名清單 `{ "models": [{ "alias", "model" }] }` |
| `GET /health` | **公開**(不檢查 token 與 Host),只回 `{"status":"ok"}`,給服務檢查用(liveness) |
| `GET /v1/status` | 需要 token:`{ "version", "records": { "dropped" }, "aliases" }`,給自己除錯用 |

**串流端點**:先等第一個項目。是錯誤 → 回對應狀態碼 + JSON 錯誤;是事件 → 回 200 並開始 SSE。
之後出錯送 `event: error` 再關閉;每 15 秒送 `: keep-alive`;不送 `[DONE]`;
沒有 `finish` / `error` 就斷線,呼叫者應視為中斷(寫進 README)。

**錯誤**:body 一律 `{"error":{"kind","message","retryable"}}`,串流的 `error` 事件同格式。

| 來源 | 狀態碼 | `kind` |
|---|---|---|
| JSON 不合法、`validate()` 失敗、未知別名、上游 400 | 400 | `invalid_request`(上游 400 附上游 body) |
| 沒帶或錯誤的 token | 401 | `unauthorized` |
| body 過大 | 413 | `payload_too_large` |
| 上游 429 | 429 + `Retry-After` | `rate_limited` |
| 同時呼叫數已滿 | 429 | `gateway_busy` |
| 上游 401 / 403(我們的 key) | 502 + tracing ERROR | `upstream_auth` |
| 上游 5xx、529、其他 4xx、連線失敗、格式錯誤 | 502 | `upstream_error` |
| 上游逾時、非串流總時間超過 | 504 | `upstream_timeout` |
| 非串流輸出到一半斷線 | 502,附 `partial` | `interrupted` |

`ApiError` 實作 axum 的 `IntoResponse`(Java 對照:`@ControllerAdvice`)。

## 18. 安全(D-017、D-024)

- `/health` 例外:不檢查任何東西,也不回傳任何資訊(D-025)。
- 其他請求依序通過:**Host 檢查**(只接受 `127.0.0.1` / `localhost` / `[::1]` 加上 port,或 `allowed_hosts`)
  → **token**(`Authorization: Bearer`,`subtle` 固定時間比較)
  → **body 上限** → handler。
- 不開 CORS;之後網頁介面要用時明確列出來源。
- 同時呼叫數用 `Semaphore::try_acquire_owned`,拿不到就 429;**permit 跟著串流走**,串流結束才歸還。
- token、API key 都是 `SecretString`;營運 log 不含內容(第一部分 §7 的四層規則照用)。
- 紀錄是完整內容(D-017):放專案外、700/600;`.gitignore` 仍加 `/records/`。
- 優雅關機:停止接受新連線 → 等進行中的請求(最多 30 秒)→ 丟掉 decorator 鏈 → 在阻塞執行緒上
  `JsonlWriter::shutdown()`(`spawn_blocking`)。**一定要明確做**:`JsonlWriter` 的 `Drop` 會 `join` 寫檔執行緒,
  不能在 tokio 工作執行緒上發生;而且 `main` 結束時其他執行緒上的物件不會執行 `Drop`,佇列裡的紀錄會直接遺失。

## 19. 測試

| 層 | 測什麼 |
|---|---|
| 單元(`llm`) | blanket impl 轉發 `complete`(用覆寫 `complete` 的假 provider 抓「改用預設值」);Alias 嚴格 / 寬鬆;Retry:重試後開始事件只送一次、輸出後不重試、401 不重試、次數用完回最後的錯誤、retry-after 超過上限直接失敗、空回答也放行(暫停時間);Record:四種 outcome、served model、延遲、取消時只送一次 |
| 單元(`JsonlWriter`) | 每天一檔、權限 700/600、佇列滿時計數、shutdown 寫完剩餘紀錄(`tempfile`) |
| 單元(`config-loader`) | `~/` 展開、不展開 `$VAR`、相對路徑以設定檔目錄為準、未知欄位的錯誤含行號、缺秘密的錯誤 |
| 單元(`gateway`) | 設定:未知欄位、各項檢查;錯誤對照表(§17 每一列);token 比對;Host 檢查 |
| E2E | 真 gateway(隨機 port)+ wiremock 假 Claude + 暫存紀錄目錄:完整與串流成功、紀錄有寫入、401、Host 拒絕、`/health` 不帶 token 也成功、未知別名 400、上游 429 → 429、上游 401 → 502、529 後重試成功、413、busy 429 |
| Benchmark | Anthropic 事件的 JSON 翻譯(第一部分 retro 帶過來的項目) |

拒絕類測試照第一部分的教訓,**指名是哪一步拒絕的**(retro §4)。

## 20. 新增的依賴

| crate | 誰用 | 用途 |
|---|---|---|
| `tokio`(正式依賴,只開 `time`) | llm | Retry 的 sleep;dev 另開 `test-util` 暫停時間 |
| `tracing` | llm、gateway | 營運 log;library 只發出事件,由 binary 決定輸出到哪 |
| `uuid`(`v7`、`serde`) | llm | 紀錄 id |
| `jiff` | llm | UTC 時間與日期 |
| `fastrand` | llm | jitter |
| `axum` 0.8 | gateway | HTTP server |
| `toml` | config-loader | 設定檔 |
| `url` | gateway | 檢查 `anthropic.base_url`;reqwest 本來就依賴同一個版本,不多編譯 |
| `subtle` | gateway | 固定時間比較 |
| `tracing-subscriber` | gateway | 營運 log 輸出到 stderr |
| `anyhow` | gateway | binary 的錯誤(CLAUDE.md 技術選型) |
| `tempfile`(dev) | llm、config-loader | 測試用暫存目錄 |

授權都在 `deny.toml` 允許範圍內,加入時以 `cargo deny` 確認。

## 21. 實作順序(一個 commit 教一個概念)

| # | 內容 | 主要 Rust 概念 | 測試 |
|---|---|---|---|
| 10 | blanket impl + `LlmProviderExt` + `Alias` | 泛型、monomorphization、blanket impl、`?Sized` | 單元 |
| 11 | `Retry` | `tokio::time`、labeled loop、事件扣住、jitter | 單元(暫停時間) |
| 12 | `CallRecord` + `RecordSink` + `Record` | `Drop`、`Instant`、UUID v7 | 單元(記憶體 sink) |
| 13 | `JsonlWriter` | OS 執行緒、`sync_channel`、`OpenOptionsExt`、`AtomicU64` | 單元(暫存目錄) |
| 14 | `crates/config-loader` | `Path` / `PathBuf`、泛型函式 `load<T: DeserializeOwned>` | 單元(暫存目錄) |
| 15 | `crates/gateway` 骨架:設定、啟動檢查、組裝 | binary crate、`anyhow`、serde 預設值、`deny_unknown_fields` | 單元 |
| 16 | HTTP API:handlers、SSE、`ApiError` | axum extractor、`IntoResponse`、`Sse` | 整合(tower `oneshot`) |
| 17 | 安全與上限:token、Host、body、並行、逾時、優雅關機 | middleware(`from_fn`)、`Semaphore`、`subtle` | 整合 |
| 18 | E2E + gateway README + `gateway.example.toml` + 翻譯 benchmark | 起真 server | E2E、bench |
| 19 | 第二部分 retro | | |

第 10~13 步都在 `crates/llm`,第 14 步是獨立的小 crate,axum 到第 16 步才出現。
第 16 步只寫 router 與測試(tower `oneshot`,不開 port);`main` 要等第 17 步加上 token 與 Host 檢查才真正啟動 server,
不讓任何一個 commit 開出沒有保護的 server。
