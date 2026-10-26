# Roadmap

> 開新 session 先讀這份。完成一步就更新「目前位置」與模組狀態。

## 使命與北極星

**使命**:Build, Test and Operate Reliable AI Systems — 從零打造可靠的 AI 系統,每一層都自己實作、量測、記錄。

**北極星**:同一個系統分三個階段長大,每個階段的產出是下一階段的原料(D-014)。

```
階段 1 個人助理  ──每天使用,產生真實紀錄──→  階段 2 資料飛輪  ──訓練出學徒模型──→  階段 3 RL
```

| 階段 | 是什麼 | 完成條件(沒達到不進下一階段) |
|---|---|---|
| **1. 個人助理** | 自己每天用的助理:介面做薄,底層做深(自寫檢索引擎、agent runtime、guardrails、MCP)。Gateway 從第一天就記錄每次呼叫 | 實際每天使用、連續數週;Eval 測試集持續累積;呼叫紀錄持續累積(本機、已遮蔽個資) |
| **2. 資料飛輪** | 從紀錄中找出簡單的請求,用大模型的輸出蒸餾出便宜的小模型;Eval 證明品質夠了,才把那類流量切過去,品質下降自動切回 | 至少一類請求改由小模型處理,品質達門檻且有統計證據;成本下降有量測數字 |
| **3. RL** | 用有標準答案的任務(測試通過、答案正確)讓小模型自己練習,超越單純模仿 | RL 訓練的模型在可驗證任務上贏過只做模仿訓練的模型 |

**原則**:根基優先於速度。每個模組都走完 CLAUDE.md 的「完成的定義」。

### 硬體(使用者的 Mac:M5 Pro、24 GB 統一記憶體)

數字都是大約值,實際依模型與設定而定。

| 階段 | 需求 | 結論 |
|---|---|---|
| 1 | Rust 編譯、Claude API、本地 embedding、本地 7~8B 4-bit 推論(約 5~6 GB) | Mac 足夠;成本只有 API 費用 |
| 2 | MLX LoRA:1.5~3B 舒適;7~8B QLoRA 吃緊但可行;14B 以上要租雲端。上線小模型用 MLX-LM / llama.cpp(vLLM 只支援 NVIDIA) | 大多 Mac 可做;學徒模型以 1.5~3B 為主 |
| 3 | RL 運算量是微調的好幾倍:0.5~1.5B 需 24 GB 級 NVIDIA GPU;7B 需 80 GB 級 | 租雲端 GPU;先用小模型跑通流程再放大 |

**雲端選項**(2026-10 查詢,多為第三方資料,用之前再確認):

- **build.nvidia.com(NIM API)**:只有推論 API,沒有訓練算力。免費額度以速率限制(約 40 RPM)計,
  條款限定開發測試用。用途:OpenAI 相容的第二個 provider、免費試用開源模型當學徒候選或評審。
- **Google Colab**:免費版 T4(16 GB)、連線不保證、閒置會斷;Pro 約 $9.99 / 100 compute units
  (A100 約 15 CU/小時)。適合階段 2 的 CUDA 實驗與階段 3 的小規模 RL;要把 checkpoint 存到雲端硬碟。
- **長時間或大模型訓練**:租專用 GPU(按小時計費)。

## 目前位置

- **模組**:001 LLM Gateway — 實作中,設計見 `docs/modules/001-llm-gateway/design.md`
- **進度**:§9 第 1~9 步全部完成,`crates/llm` 完成(SSE benchmark:428 MiB/s,不需要優化)
- **下一步**:寫 001 第一部分(`crates/llm`)的 `retro.md`,再設計 001 第二部分 `crates/gateway`(axum、呼叫紀錄、模型別名)
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
| D-012 | 授權 MIT;程式碼註解一律中文 | 使用者選擇;中文註解方便學習 | 2026-10-24 |
| D-013 | 定位:學習專案、不發布 crates.io,但成果公開分享;每個模組做到「完整」(見 CLAUDE.md 完成的定義);主題不限於 RAG | 使用者希望過程完整、別人能看能用,但不要搞得太複雜 | 2026-10-24 |
| D-014 | 北極星 = 三階段同一系統:個人助理 → 資料飛輪 → RL;每階段有可量化的完成條件 | 後一階段需要前一階段的產出(真實紀錄、學徒模型);使用者要有難度、根基優先 | 2026-10-24 |
| D-015 | 每一步通過 fmt、clippy、測試、私有資料掃描後,Claude 自動 commit + push,再給導讀 | 使用者重視流程順暢;只在設計決策時參與 | 2026-10-24 |
| D-016 | 常數放在使用它的 module;第 7 步加 `Limits` struct 經 constructor 注入;設定檔(`gateway.toml`)只由 gateway binary 讀,library 不讀檔案也不讀環境變數 | 常數跟著用途走;設定由 composition root 決定,library 才可測試、沒有隱藏行為 | 2026-10-24 |

## 模組

### 階段 1:個人助理

| # | 模組 | 層 | 狀態 |
|---|---|---|---|
| 001 | LLM Gateway(`crates/llm` + `crates/gateway`,含呼叫紀錄) | Rust 服務 | 第一部分完成(9/9);gateway 待設計 |
| 002 | Eval Harness(用助理的真實案例累積測試集) | Rust 服務 | — |
| 003 | Agent runtime + Tool calling + MCP | Rust 服務 | — |
| 004 | 檢索引擎(BM25 / HNSW 從零)+ RAG | Rust 服務 | — |
| 005 | 個人助理應用(把 001~004 組起來,開始每天用) | 應用 | — |

### 階段 2:資料飛輪

| # | 模組 | 層 | 狀態 |
|---|---|---|---|
| 006 | 紀錄儲存與分析(找出可交給小模型的請求類別) | Rust 服務 | — |
| 007 | 訓練資料整理(蒸餾、合成資料、個資遮蔽、去重) | Python 模型 | — |
| 008 | LoRA / QLoRA 微調學徒模型(MLX) | Python 模型 | — |
| 009 | 小模型推論服務(MLX-LM / llama.cpp,OpenAI 相容) | Python 模型 | — |
| 010 | 路由器:Jev(現成決策模型)、自己訓練的分類模型、規則,三者用 Eval 比較 | Rust 服務 + Python 模型 | — |
| 011 | 灰度切換與自動回滾(依 Eval 統計證據決定) | Rust 服務 | — |

**比較基準要包含 Haiku**:自己蒸餾的學徒模型,在品質與成本上必須贏過「直接把簡單請求交給
`claude-haiku-5-5`」(比 Sonnet 便宜約 20 倍);贏不過的話,飛輪就沒有意義,直接路由給 Haiku 即可。

### 階段 3:RL

| # | 模組 | 層 | 狀態 |
|---|---|---|---|
| 012 | RL 環境與判分器(高速執行任務、驗證答案) | Rust 服務 | — |
| 013 | GRPO 訓練(先 0.5~1.5B 跑通,再放大) | Python 模型 | — |
| 014 | RL 學徒 vs 模仿學徒的比較與上線 | 全層 | — |

**001 的提醒**:`crates/gateway` 那部分要從第一天就記錄每次呼叫(輸入、輸出、用量、延遲),
只存本機、先經過個資遮蔽(design.md §7),這是階段 2 的原料。

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

- **004 的範圍**:到 004 時再設計。做法以公開的技術為基礎從頭設計,目標是比使用者既有的實作更完整。
- **Jev**:TypeSafe 的決策模型(非生成式,從固定選項挑一個並給機率)。目前資料多為第三方文章,
  到 010 時先查官方文件確認 API 與用法。
- **模型別名(設計 `crates/gateway` 時決定)**:不要每多一種用途就加一個環境變數。提案是在
  `gateway.toml` 定義具名別名(例如 `default`、`cheap`、`judge`),請求寫別名、gateway 換成真正的
  model ID;換模型只改設定檔、不改程式。「依請求內容動態挑模型」是 010 路由器的工作。
  目前只有兩個變數:`LLM_MODEL`(範例)、`LIVE_MODEL`(live test,預設 Haiku)。

## 想法池(還沒排進模組)

**其他有難度的方向**(可當北極星之後的延伸,或某個模組的進階版):

- **Rust 推論引擎**:continuous batching、paged KV cache、speculative decoding;可替換 009。
- **Durable Agent Runtime**:當機後從中斷點恢復、tool 沙箱、多 agent 協作;可深化 003。
- **向量資料庫從零寫**:HNSW、量化壓縮、過濾、持久化;可深化 004。
- **受限解碼引擎**:逐 token 擋掉不合法輸出,保證 JSON / 文法正確。
- **可信任的 AI 評審**:訓練並校準一個能證明自己可信的評審模型;可深化 002 與 011。
- **最底層選修**:BPE tokenizer 從零寫、小型 Transformer 從零寫(理解原理用)。

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
