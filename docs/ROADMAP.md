# Roadmap

> 開新 session 先讀這份。完成一步就更新「目前位置」與模組狀態。

## 目前位置

- **模組**:001 LLM Gateway — 設計中(尚未寫任何程式)
- **下一步**:討論 Gateway 的範圍與第一個設計分岔(crate 結構、provider 抽象方式)
- **最後更新**:2026-10-24

## 決策

| # | 決策 | 理由 | 日期 |
|---|---|---|---|
| D-001 | Rust 重起爐灶,Go 只當參考 | 學習順序改為由上往下;逐檔移植會先卡在 harness 底層 | 2026-10-24 |
| D-002 | 三層架構:Python 模型層 → Rust 服務層 → 應用層 | Rust 負責效能,Python 負責訓練生態;層間只靠 HTTP/gRPC | 2026-10-24 |
| D-003 | 不用 GSD,每模組只寫 design + retro | Go 專案文件:程式 ≈ 17:1,回饋太慢 | 2026-10-24 |
| D-004 | Claude 寫、使用者讀;架構由使用者選 | 使用者目標是 Rust 語法 + 架構設計,寫 Rust 目前慢 | 2026-10-24 |
| D-005 | 第一個模組 = LLM Gateway | 最好的 Rust 入門(async/serde/錯誤/streaming),也是後面所有模組的地基 | 2026-10-24 |

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
