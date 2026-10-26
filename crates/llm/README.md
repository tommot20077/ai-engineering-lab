# llm

與 provider 無關、以串流為核心的 LLM client。所有上層(gateway、agent、eval)都透過它呼叫模型,
不直接碰任何廠商的 API。設計與取捨見 [`docs/modules/001-llm-gateway/design.md`](../../docs/modules/001-llm-gateway/design.md)。

目前支援的 provider:**Anthropic**(Messages API,`anthropic-version: 2026-10-21`)。

## 快速開始

```bash
cp .env.example .env          # 在 repo 根目錄,填入 ANTHROPIC_API_KEY
cargo run -p llm --example chat -- "用一句話介紹 Rust"
```

回答會一邊產生、一邊印出來;結束時在 stderr 印出停止原因與 token 用量。

## 用法

```rust
use futures::StreamExt;
use llm::{AnthropicClient, AnthropicConfig, LlmProvider, SecretString, StreamEvent};

let client = AnthropicClient::new(AnthropicConfig::new(SecretString::from(api_key)))?;

// 一次拿到完整回答
let response = client.complete(request.clone()).await?;

// 或者一邊收、一邊處理
let mut events = client.stream(request);
while let Some(event) = events.next().await {
    if let StreamEvent::TextDelta { text, .. } = event? {
        print!("{text}");
    }
}
```

- **API key 從哪裡來由呼叫者決定**:library 不讀環境變數、不讀設定檔。key 包在 `SecretString` 裡,
  `Debug` 只會印出 `[REDACTED]`。
- **各家專屬的設定**放在 `Request::provider_options`,以 provider 名稱為 key,例如
  `{"anthropic": {"output_config": {"effort": "low"}}}`。
- **錯誤**都是 `LlmError`;`is_retryable()` 告訴你值不值得重試。已經輸出內容之後才斷線會回傳
  `Interrupted { partial, cause }`,不應該自動重試。

## 架構

```
Request ──translate──→ Anthropic JSON ──HTTP──→ api.anthropic.com
                                                       │ SSE
Response ←─Accumulator←─ StreamEvent ←─translate←─ SseParser
```

| 檔案 | 職責 |
|---|---|
| `types.rs` / `stop.rs` | 中立的對話型別、停止原因 |
| `event.rs` | 中立的串流事件 |
| `error.rs` | 錯誤型別與可重試分類 |
| `sse.rs` | SSE 解析器(跟 provider 無關) |
| `accumulate.rs` | 事件流 → 完整回應,同時檢查協定 |
| `provider.rs` | `LlmProvider` trait、中途失敗規則 |
| `anthropic/` | Anthropic 的 JSON 形狀、翻譯、HTTP client(對外只公開 client) |

## 測試

```bash
cargo test -p llm                                    # 單元 + 整合測試(假的 HTTP 伺服器)
cargo test -p llm --test live_anthropic -- --ignored # 打真的 API,需要 .env,會花一點錢
```

| 種類 | 位置 | 內容 |
|---|---|---|
| 單元 | `src/**` 的 `#[cfg(test)]` | 純邏輯:翻譯、解析、累加、錯誤分類 |
| Property | `src/sse.rs` | 任意位元組不 panic;不管網路怎麼切,解出來的事件都一樣 |
| Snapshot | `src/anthropic/snapshots/` | 送出的請求 body 不意外變動 |
| 整合 | `tests/anthropic_http.rs` | 走真的 HTTP(wiremock):串流、429、401、逾時、中途斷線 |
| Live | `tests/live_anthropic.rs` | 真的 Claude:兩輪對話、錯誤的 key |

## 效能

`cargo bench -p llm --bench sse`。量測環境:Apple M5 Pro,release build,2026-10-26。

| 項目 | 輸入 | 時間 | 吞吐量 |
|---|---|---|---|
| SSE 解析,一次全部餵入 | 一則 1000 個 text_delta 的回答(約 137 KB) | 307 µs | 428 MiB/s |
| SSE 解析,切成 1 KB | 同上 | 315 µs | 417 MiB/s |
| SSE 解析,切成 16 bytes(最壞情況) | 同上 | 351 µs | 374 MiB/s |
| 累加器 | 1000 個 `TextDelta` → `Response` | 12.5 µs | 80 M 事件/s |

**結論:不需要優化。** 這樣一則回答,Claude 要花好幾秒才串流完,我們解析它只要 0.3 ms;
就算網路把資料切成 16 bytes 的碎片,也只慢約 13%。換算下來,單一 CPU 核心的解析能力
大約能應付上萬條同時進行的串流。逐 byte 處理的簡單寫法可以保留。

尚未量測:Anthropic 事件的 JSON 解析(`anthropic/translate.rs`,不是公開 API),
會在 gateway 的端到端 benchmark 一起量。
