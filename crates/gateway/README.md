# gateway

把 [`llm`](../llm/README.md) 包成本機的 HTTP 服務:一個地方管 API key、模型別名、重試與上限,
並把**每一次呼叫完整記錄下來**(之後訓練小模型的原料,D-014、D-021)。
設計與取捨見 [`design-part2.md`](../../docs/modules/001-llm-gateway/design-part2.md)。

```
你的程式 ──HTTP──→ gateway ──→ Record → Retry → Alias → AnthropicClient ──HTTPS──→ Claude
                     │                │
                 Host / token     每次呼叫一行 JSONL
```

## 快速開始

```bash
# 在 repo 根目錄
cp .env.example .env                  # 填 ANTHROPIC_API_KEY;GATEWAY_TOKEN 用 openssl rand -hex 32 產生
cp gateway.example.toml gateway.toml  # 預設值就能用:只聽 127.0.0.1:8787
cargo run -p gateway --release        # 或 -- --config <path>
```

啟動時會印出檢查結果,特別是**紀錄寫在哪裡**:

```
gateway: startup checks passed
  config   /Users/you/ai-fundamentals/gateway.toml
  bind     127.0.0.1:8787 (local only)
  aliases  cheap → claude-haiku-5-5, default → claude-sonnet-5-5
  records  /Users/you/Library/Application Support/ai-engineering-lab/gateway/records
  retry    up to 3 attempts, waits 1000..10000 ms
listening on http://127.0.0.1:8787 (Ctrl+C to stop)
```

設定有錯會一次列出全部的問題,然後拒絕啟動。停止:Ctrl+C 或 SIGTERM,會等進行中的請求做完(最多 30 秒)、
把紀錄寫完才結束。

## 呼叫

```bash
# 只把 token 讀進目前的 shell(不要整份 source .env,API key 不需要出現在 shell 裡)
export GATEWAY_TOKEN=$(grep '^GATEWAY_TOKEN=' .env | cut -d= -f2-)

curl -s localhost:8787/v1/chat \
  -H "Authorization: Bearer $GATEWAY_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"model": "cheap", "max_tokens": 256,
       "messages": [{"role": "user", "content": [{"type": "text", "text": "用一句話介紹 Rust"}]}]}'
```

```json
{"id":"msg_01…","model":"claude-haiku-5-5","content":[{"type":"text","text":"Rust 是…"}],
 "stop":{"kind":"end_turn","raw":"end_turn"},
 "usage":{"input_tokens":17,"output_tokens":42,"cache_read_tokens":0,"cache_write_tokens":0}}
```

`model` 只能寫 `gateway.toml` 裡 `[aliases]` 的別名,寫真的模型名稱會回 400。換模型只要改設定檔,呼叫端不用動。

### 請求格式

body 就是 `llm::Request` 的 JSON。必填的只有 `model`、`messages`、`max_tokens`:

| 欄位 | 說明 |
|---|---|
| `model` | 別名 |
| `messages` | `[{"role": "user" \| "assistant", "content": [區塊…]}]`;區塊有 `text`、`thinking`、`tool_call`、`tool_result` |
| `max_tokens` | 必須大於 0 |
| `system` | 選填,system prompt |
| `tools` | 選填,`[{"name", "description", "input_schema"}]` |
| `provider_options` | 選填,各家專屬設定,以 provider 名稱為 key,例如 `{"anthropic": {"output_config": {"effort": "low"}}}` |

## 端點

| 端點 | 需要 token | 回應 |
|---|---|---|
| `POST /v1/chat` | 是 | 完整回答(JSON)。最多等 10 分鐘,長回答請用串流 |
| `POST /v1/chat/stream` | 是 | SSE,見下面 |
| `GET /v1/models` | 是 | `{"models": [{"alias": "cheap", "model": "claude-haiku-5-5"}, …]}` |
| `GET /v1/status` | 是 | 版本、別名數、紀錄是否開啟、因為佇列滿而遺失的紀錄數 |
| `GET /health` | **否** | `{"status":"ok"}`,給服務檢查用,不回傳任何其他資訊 |

## 串流(SSE)

```bash
curl -sN localhost:8787/v1/chat/stream -H "Authorization: Bearer $GATEWAY_TOKEN" \
  -H 'content-type: application/json' -d '{"model": "cheap", "max_tokens": 256, "messages": […]}'
```

```
event: message_start
data: {"type":"message_start","id":"msg_1","model":"claude-haiku-5-5"}

event: text_start
data: {"type":"text_start","index":0,"meta":{}}

event: text_delta
data: {"type":"text_delta","index":0,"text":"Hel"}

event: text_delta
data: {"type":"text_delta","index":0,"text":"lo"}

event: text_end
data: {"type":"text_end","index":0,"meta":{}}

event: finish
data: {"type":"finish","stop":{"kind":"end_turn","raw":"end_turn"},"usage":{…},"meta":{}}
```

- `event:` 是事件種類;`data:` 是 `llm::StreamEvent` 的 JSON(自帶 `type`),Rust 呼叫者可以直接反序列化。
- 其他事件:`thinking_start/delta/end`、`tool_call_start`、`tool_args_delta`、`tool_call_end`。
  `meta` 是 provider 的資料(例如 thinking 的簽章),**下一輪要原樣送回**。
- **狀態碼在第一個事件之前決定**:一開始就失敗(429、上游錯誤…)會收到正常的 HTTP 錯誤與 JSON,不是 200。
- 已經開始輸出之後才出錯:送一個 `event: error`(格式同下面的錯誤 JSON),然後關閉。
- 每 15 秒一行 `: keep-alive` 註解,避免中間的 proxy 切斷安靜的連線;SSE client 會自動忽略。
- 沒有 `[DONE]`。**正常結束一定以 `finish` 或 `error` 收尾**;連線在那之前斷掉,請當成中斷處理。
- 呼叫端斷線 → gateway 取消上游請求,紀錄記成 `cancelled`。

## 錯誤

body 一律是同一個形狀,串流的 `error` 事件也一樣:

```json
{"error":{"kind":"upstream_auth","message":"provider returned HTTP 401","retryable":false}}
```

程式請看 `kind` 與 `retryable`,`message` 是給人看的。

| 狀態碼 | `kind` | 什麼時候 | 該怎麼辦 |
|---|---|---|---|
| 400 | `invalid_request` | JSON 不合法、未知別名、`max_tokens` 為 0、上游說請求不合法(附 `detail`) | 改請求 |
| 401 | `unauthorized` | 沒帶或帶錯 token(附 `WWW-Authenticate: Bearer`) | 檢查 `GATEWAY_TOKEN` |
| 403 | `invalid_host` | `Host` 不在允許清單(擋 DNS rebinding) | 用 `localhost` / `127.0.0.1` 連,或設定 `allowed_hosts` |
| 413 | `payload_too_large` | body 超過 `limits.max_body_bytes` | |
| 429 | `rate_limited` | 上游限流,附 `Retry-After` | 等那麼久再試 |
| 429 | `gateway_busy` | 同時進行的請求達到 `limits.max_concurrent` | 稍後再試 |
| 502 | `upstream_auth` | 上游拒絕 **gateway 的** API key | 不是呼叫端的錯:檢查 `.env` 的 `ANTHROPIC_API_KEY` |
| 502 | `upstream_error` | 上游 5xx / 529、連線失敗、回應格式錯誤(已經重試過) | 看 `retryable` |
| 502 | `interrupted` | 非串流請求輸出到一半斷線,附 `partial`(斷線前收到的部分) | 自己決定要不要用 `partial` |
| 504 | `upstream_timeout` | 上游太久沒回應,或非串流超過 10 分鐘 | 改用串流 |

上游 529 / 5xx / 連線失敗會在**還沒輸出任何內容之前**自動重試(`[retry]`);已經輸出之後不重試,避免重複的內容。

## 設定

全部寫在 `gateway.toml`(範例與說明見 [`gateway.example.toml`](../../gateway.example.toml));秘密只放 `.env`。

- 打錯的欄位名稱會讓 gateway 拒絕啟動,不會被默默忽略。
- 相對路徑以**設定檔所在的目錄**為準;`~/` 會展開成家目錄。
- `bind` 的 port 寫 0 = 讓作業系統挑一個空的 port(測試用),啟動訊息會印出實際的 port。
- 要讓其他機器連:`bind` 改成對外位址,**同時**設定 `allow_remote = true` 和 `allowed_hosts`(呼叫端連線用的 `host:port`)。
  gateway 本身只有明文 HTTP,對外請放在 TLS reverse proxy 後面。

## 紀錄

每次呼叫一行 JSON(`llm::CallRecord`):請求、回應、結果(`ok` / `error` / `interrupted` / `cancelled`)、
用量、延遲。每天(UTC)一個檔案,例如 `2026-10-31.jsonl`。

- **內容是完整的對話**:預設放在專案外的資料目錄,目錄權限 700、檔案 600;權限太寬會拒絕啟動。
- 寫檔在獨立的執行緒,不拖慢請求;佇列滿了會丟掉紀錄並計數(`/v1/status` 的 `dropped`)。
- 不要記錄:`[record] enabled = false`。

## 原始碼

| 檔案 | 職責 |
|---|---|
| `src/main.rs` | 唯一讀環境變數與設定檔的地方;啟動、優雅關機 |
| `src/config.rs` | `RawConfig`(檔案原樣)→ 檢查 → `Settings` |
| `src/startup.rs` | 組裝 decorator 鏈與紀錄 writer |
| `src/api.rs` | 路由、handler、SSE、並行上限 |
| `src/guard.rs` | Host 檢查、token 檢查 |
| `src/error.rs` | `LlmError` → HTTP 狀態碼與錯誤 JSON |

## 測試

```bash
cargo test -p gateway
```

| 種類 | 位置 | 內容 |
|---|---|---|
| 單元 | `src/**` 的 `#[cfg(test)]` | 設定檢查、錯誤對照表、組裝順序 |
| Router | `src/api.rs`、`src/guard.rs` | 整條路由用 tower `oneshot`,不開 port:SSE、上限、並行、Host / token |
| E2E | `tests/e2e.rs` | 真的執行 gateway 程式,上游換成 wiremock:端到端呼叫、重試、錯誤對照、SIGTERM 優雅關機與紀錄寫完 |
