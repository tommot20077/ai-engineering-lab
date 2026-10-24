# AI Engineering Lab(Rust)

以學習為核心的 AI 工程實驗室。前身是 Go 專案 `/Users/luca/開發/workspace/go`
(`modules/agentrt`,一個手寫 agent runtime),**已決定在 Rust 重起爐灶,Go 只當參考資料**,
不逐檔移植。

**每次開新 session,先讀 [`docs/ROADMAP.md`](docs/ROADMAP.md)** — 目前進度、已做決策、
待辦都在那裡。完成一步就更新它。

## 三層架構(目標)

```
應用層         CLI / Web / Bot …(先做出看得見的東西)
Rust 服務層    Gateway · RAG · Agent · MCP · Eval …(主力,追求效能)
Python 模型層  微調 · embedding/reranker · 推論服務 → 以 HTTP/gRPC 提供給 Rust
```

層與層之間只靠網路協定溝通,不互相嵌入。

## 學習模式(使用者明確要求)

使用者會讀程式(Java、Go 背景),但寫 Rust 慢。目標是學 **Rust 語法/慣用法** 與 **架構設計**。

- **Claude 寫、使用者讀。** 一個 commit 只教一個概念,約 100~200 行。`git log` 本身就是教材。
- **架構由使用者決定。** 遇到設計分岔,先列 2~3 個方案與取捨,等使用者選,再寫。
- **註解寫 Rust 特有的東西**,盡量對照 Go/Java(例:「這裡要 `Arc`,因為 Go 有 GC、Rust 沒有」)。
  業務邏輯註解保持精簡,不要寫得比程式長。
- **每步附導讀**:看哪幾行、為什麼這樣寫、Go 會怎麼寫。
- **可選小練習**(5~15 分鐘):讓使用者改一點東西、跟著 compiler 錯誤走。
- 使用者撞到 compiler 錯誤時,解釋錯誤本身,而不是直接給答案。

## 流程(刻意輕量,不用 GSD)

Go 專案的規劃文件 : 程式 ≈ 17 : 1,拖慢學習回饋,所以這裡不用 GSD。

- 每個模組只有兩份文件,放 `docs/modules/NNN-名稱/`:
  - `design.md` — 寫之前,半頁:要解決什麼、選了哪個方案、為什麼。
  - `retro.md` — 寫完後:學到的 Rust 概念、架構決策、文章大綱。
- 跨模組的決策記在 `docs/ROADMAP.md` 的決策表。
- Commit 只在使用者要求時做。

## 技術選型(預設,遇到再確認)

- Rust edition 2024,toolchain 1.99+。
- async:`tokio`;HTTP client:`reqwest`;HTTP server:`axum`;序列化:`serde`;
  錯誤:`thiserror`(library)/ `anyhow`(binary)。
- Anthropic/OpenAI 沒有官方 Rust SDK → transport 自己用 `reqwest` 寫,本身就是學習內容。
- 核心機制從零實作,不套 AI 框架(延續 Go 專案 D-003)。

## Go 專案值得回收的設計

provider-agnostic seam(`llm.Model`)、未知 stop reason fail-closed、tool 參數當不可信輸入、
tool call 帶 idempotency ID、loop 有引擎自有的硬性終止護欄。參考
`/Users/luca/開發/workspace/go/modules/agentrt/`。
