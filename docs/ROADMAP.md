# Roadmap

> 開新 session 先讀這份。完成一步就更新「目前位置」與模組狀態。

## 目前位置

- **模組**:001 LLM Gateway — 設計中(尚未寫任何程式)
- **下一步**:寫 `docs/modules/001-llm-gateway/design.md`,彙整 D-006~D-011,並對剩下的項目(錯誤型別與重試分類、串流中途失敗語意、密鑰處理、執行期 dispatch、Request/Message 型別)提出預設方案給使用者審
- **最後更新**:2026-10-24

## 決策

| # | 決策 | 理由 | 日期 |
|---|---|---|---|
| D-001 | Rust 重起爐灶,Go 只當參考 | 學習順序改為由上往下;逐檔移植會先卡在 harness 底層 | 2026-10-24 |
| D-002 | 三層架構:Python 模型層 → Rust 服務層 → 應用層 | Rust 負責效能,Python 負責訓練生態;層間只靠 HTTP/gRPC | 2026-10-24 |
| D-003 | 不用 GSD,每模組只寫 design + retro | Go 專案文件:程式 ≈ 17:1,回饋太慢 | 2026-10-24 |
| D-004 | Claude 寫、使用者讀;架構由使用者選 | 使用者目標是 Rust 語法 + 架構設計,寫 Rust 目前慢 | 2026-10-24 |
| D-005 | 第一個模組 = LLM Gateway | 最好的 Rust 入門(async/serde/錯誤/streaming),也是後面所有模組的地基 | 2026-10-24 |
| D-006 | Gateway 先做 library crate(`crates/llm`),再包 server(`crates/gateway`);Cargo workspace | 核心邏輯與傳輸方式分離;Agent/Eval 可直接依賴 `llm` | 2026-10-24 |
| D-007 | 完整測試金字塔 + eval 分離(見 CLAUDE.md「測試策略」) | 使用者重視架構、效能、安全;根基優先於速度 | 2026-10-24 |
| D-008 | 第一個 adapter = Anthropic(raw HTTP via `reqwest`,`anthropic-version: 2026-10-21`,預設模型 `claude-sonnet-5-5`,使用者指定) | 使用者手上有 Claude API key;Rust 無官方 SDK | 2026-10-24 |
| D-009 | Provider trait streaming-first:核心是 `stream()`,`complete()` 為收集 stream 的預設方法;事件 enum 自己定義為中立格式;不支援 streaming 的 provider 把整份回應包成一批事件 | 單一資料路徑;可中途截斷;首字延遲低。Embedding/Rerank 另立 trait(介面隔離) | 2026-10-24 |
| D-010 | 事件 enum = 語意事件 + index,參考 Vercel AI SDK:每種段落(文字/思考/工具)都有 start/delta/end;每個事件可帶 opaque provider metadata;StopReason 同時保留 unified 與 raw;錯誤是串流中的一種事件 | 使用者端最好用、各家好翻譯;opaque 欄位保住必須原樣回傳的資料(如 thinking 簽章) | 2026-10-24 |
| D-011 | 對外格式與內部格式分離:之後在 gateway **額外開**一個 OpenAI 相容端點(與自家原生 API 並存,不是取代),做成「對外 adapter」(中立事件 → OpenAI chunk),`llm` crate 不動 | Ports & Adapters;內部保留完整資訊,對外相容現成 client | 2026-10-24 |

## 模組

| # | 模組 | 層 | 狀態 |
|---|---|---|---|
| 001 | LLM Gateway | Rust 服務 | 設計中 |
| 002 | Eval Harness | Rust 服務 | — |
| 003 | Agent + Tool calling + MCP | Rust 服務 | — |
| 004 | 檢索引擎(BM25/HNSW 從零)+ 進階 RAG | Rust 服務 | — 待使用者提供 common RAG |
| 005 | Embedding / Reranker 微調 | Python 模型 | — |
| 006 | LoRA、量化、推論服務 | Python 模型 | — |
| 007 | 整合專案:微調 embedding → Eval 證明召回提升 → Gateway 對外 → 應用 | 全層 | — |

順序可調,但 002 Eval 要早做 — 之後每個「變好了」都要靠它量化。

### 001 Gateway 預計會學到的 Rust 概念(大致順序)

1. `struct` / `enum` / `match` + `serde`
2. `Result` / `?` / `thiserror`
3. `trait`(provider 抽象,對照 Go `llm.Model`)
4. async + `tokio`
5. `reqwest` 打 HTTP
6. SSE streaming 解析
7. `axum` 對外提供 API
8. `Arc` / `Mutex` 共享狀態
9. lifetime、generics

## 待辦 / 等使用者

- **common RAG 程式碼**:使用者之前寫過,之後會提供(不是近期)。拿到後先讀,
  再決定 004 從哪裡開始、哪些基礎可以跳過。

## 想法池(還沒排進模組)

知識層除了 RAG 的其他做法:

- **LLM Wiki**(Karpathy, 2026-04):LLM 把原始資料「編譯」成互相連結的 markdown wiki,
  查詢時讀 wiki 而不是每次重新檢索 chunk。適合中小規模、會持續累積的知識。
- **Agentic search**:不用向量庫,讓 agent 用 grep/讀檔自己找(Claude Code 的做法)。
- **CAG(Cache-Augmented Generation)**:資料量夠小就整包塞進 context + prompt caching。
- **GraphRAG / LightRAG**:抽實體與關係建知識圖譜,回答跨文件的全局問題。
- **Agent Memory**(MemGPT/Letta、Mem0):長期記憶的寫入、更新、遺忘。
- **Contextual Retrieval**:chunk 前先讓 LLM 補上下文描述再做 embedding。
- **Text-to-SQL**:結構化資料不走 RAG,直接生成查詢。
- **多模態 RAG**(ColPali):PDF 直接當圖片檢索,不先轉文字。
- **Late interaction**(ColBERT):多向量檢索,精度介於 bi-encoder 和 reranker 之間。
- **Skills / 漸進揭露**:知識按需載入(Anthropic Agent Skills 的模式)。
