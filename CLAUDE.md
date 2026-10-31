# AI Engineering Lab(Rust)

以學習為核心的 AI 工程實驗室。前身是 Go 專案 `/Users/luca/開發/workspace/go`
(`modules/agentrt`,一個手寫 agent runtime),**已決定在 Rust 重起爐灶,Go 只當參考資料**,
不逐檔移植。

**每次開新 session,先讀 [`docs/ROADMAP.md`](docs/ROADMAP.md)** — 目前進度、已做決策、
待辦都在那裡。完成一步就更新它。

**定位(D-013)**:這是學習專案,不發布到 crates.io;但成果公開分享,所以每個模組都要做「完整」—
別人 clone 下來看得懂、跑得起來。RAG 是主題之一,不是唯一。

**完成的定義**(每個模組都要有):`design.md` → 實作 + 測試 → crate 的 `README.md`
(做什麼、怎麼跑、怎麼用)+ 可執行的 `examples/` → CI 綠燈 → `retro.md`。

**私有資料**:使用者本機有私有參考資料,記在 `CLAUDE.local.md`(已 gitignore)。
那裡的內容一律不准寫進 repo、commit 訊息、PR 或 issue。

## 三層架構(目標)

```
應用層         CLI / Web / Bot …(先做出看得見的東西)
Rust 服務層    Gateway · RAG · Agent · MCP · Eval …(主力,追求效能)
Python 模型層  微調 · embedding/reranker · 推論服務 → 以 HTTP/gRPC 提供給 Rust
```

層與層之間只靠網路協定溝通,不互相嵌入。

## 原則(使用者明確要求)

**架構乾淨、效能、安全、高擴展性。不急著看成果,根基要打穩。**

- 擴展點只開在已知會變的地方(provider、middleware、儲存後端),用 trait 劃邊界;
  不為假想需求預先抽象。
- 外部輸入(API 回應、SSE、tool 參數、使用者請求)一律當不可信:驗證、設上限。
- **失敗姿態看後果**:讀取路徑 fail-closed(條件不明就回空,不回全部);
  寫入路徑 fail-loud(不合法就立刻報錯,不要污染已存的資料)。
- 密鑰不進 log、不進 `Debug` 輸出;`#![forbid(unsafe_code)]` 為預設。
- 效能主張要有 benchmark 數字撐,不憑感覺。

## 測試策略

| 層 | 工具 | 測什麼 | 何時跑 |
|---|---|---|---|
| 單元 | `#[cfg(test)]` | 純邏輯:wire ↔ domain 翻譯、SSE parser、錯誤分類 | 每次 |
| Snapshot | `insta` | 送出的 request JSON 不意外變動 | 每次 |
| Property / Fuzz | `proptest`、`cargo-fuzz` | parser 遇到任意輸入不 panic | 每次 / 手動 |
| 整合 | `tests/` + `wiremock` | adapter 走真 HTTP:streaming、timeout、429/5xx | 每次 |
| Live | `#[ignore]` | 打真 API,驗證對方格式沒變 | 手動 `-- --ignored` |
| E2E | 起真 server(隨機 port) | client → gateway → fake provider 全鏈路 | 每次 |
| Benchmark | `criterion` | 熱路徑效能,有數字才談優化 | 手動 |

- Mock 網路邊界,不 mock parser(沿用 Go 專案原則)。
- 工具在第一次需要時才加,不預先全裝。
- **Eval 不是測試**:測試是確定性的 pass/fail;eval 是對資料集打分、看趨勢。
  Eval 放獨立 crate,CI 只跑小型 smoke set,完整 eval 手動跑並保存結果。
- CI(GitHub Actions):`fmt`、`clippy -D warnings`、`test`、`cargo deny`(供應鏈與授權)。

## 學習模式(使用者明確要求)

使用者會讀程式(Java、Go 背景),但寫 Rust 慢。目標是學 **Rust 語法/慣用法** 與 **架構設計**。

- **Claude 寫、使用者讀。** 一個 commit 只教一個概念,`git log` 本身就是教材。
  行數不設限,不必為了大小拆檔或解釋;流程順暢比行數重要。
- **架構由使用者決定。** 遇到設計分岔,先列 2~3 個方案與取捨,等使用者選,再寫。
- **程式碼註解一律用中文**(D-012)。
- **註解寫 Rust 特有的東西**,**優先對照 Java**(使用者最熟,含 Java 21 的 sealed、record、
  pattern matching switch);Go 只在明顯更貼切時才用。例:「這裡要 `Arc`,因為 Java 有 GC、Rust 沒有」。
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
- **每一步做完自動 commit + push(D-015)**:fmt、clippy、測試、私有資料掃描全部通過後,
  Claude 直接 commit 並 push,然後給導讀;使用者只在設計決策時參與。有問題就在下一個 commit 修。
- **每一步照 `lab-step` skill 走**(`.claude/skills/lab-step/`):TDD → 空殼上確認失敗 → 實作 →
  mutation 檢查(`scripts/mutate.py`)→ `scripts/check.sh` → 文件 → commit / push / CI → 導讀。
- **私有資料掃描由 git hook 強制**(`.githooks/`,新 clone 先跑 `scripts/install-hooks.sh`):
  pre-commit 掃暫存區、commit-msg 掃訊息。關鍵字在 gitignore 的 `.private-patterns`。不准用 `--no-verify` 繞過。

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
