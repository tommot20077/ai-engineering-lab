# AI Engineering Lab(Rust)

[![CI](https://github.com/tommot20077/ai-engineering-lab/actions/workflows/ci.yml/badge.svg)](https://github.com/tommot20077/ai-engineering-lab/actions/workflows/ci.yml)

**Build, Test and Operate Reliable AI Systems** — 從零打造可靠的 AI 系統,每一層都自己實作、量測、記錄。

這是一個學習專案:不套 AI 框架,LLM client、gateway、eval、agent runtime、檢索引擎都自己寫,
目的是弄懂每一層在做什麼、怎麼測、怎麼量。不發布到 crates.io,但每個模組都做到「別人 clone 下來看得懂、跑得起來」。

## 目標:同一個系統分三個階段長大

```
階段 1 個人助理  ──每天使用,產生真實紀錄──→  階段 2 資料飛輪  ──訓練出學徒模型──→  階段 3 RL
```

1. **個人助理**:自己每天用的助理。介面做薄,底層做深;gateway 從第一天就記錄每一次呼叫。
2. **資料飛輪**:從紀錄裡找出簡單的請求,蒸餾出便宜的小模型;Eval 證明品質夠了才切換流量,品質下降自動切回。
3. **RL**:用有標準答案的任務(測試通過、答案正確)讓小模型自己練習,超越單純模仿。

## 三層架構

```
應用層         CLI / Web / Bot …
Rust 服務層    Gateway · Eval · Agent · RAG · MCP …     ← 目前在這一層
Python 模型層  微調 · embedding / reranker · 推論服務
```

層與層之間只靠網路協定(HTTP / gRPC)溝通,不互相嵌入。

## 模組

| # | 模組 | 狀態 |
|---|---|---|
| 001 | **LLM Gateway**:provider 抽象、串流、重試、呼叫紀錄、本機 HTTP 服務 | ✅ 完成 |
| 002 | Eval Harness:用真實案例累積測試集、打分、看趨勢 | 設計完成([design](docs/modules/002-eval-harness/design.md)) |
| 003 | Agent runtime + tool calling + MCP | 規劃中 |
| 004 | 檢索引擎(BM25 / HNSW 從零)+ RAG | 規劃中 |
| 005 | 個人助理應用(把 001~004 組起來) | 規劃中 |

階段 2、3 的模組(006~014)與所有架構決策見 [`docs/ROADMAP.md`](docs/ROADMAP.md)。

### 目前有的 crate

| crate | 是什麼 | 說明 |
|---|---|---|
| [`llm`](crates/llm/README.md) | 與 provider 無關、以串流為核心的 LLM client;decorator(別名、重試、紀錄) | library |
| [`gateway`](crates/gateway/README.md) | 把 `llm` 包成本機 HTTP 服務:token / Host 檢查、上限、SSE、優雅關機,每次呼叫寫一行 JSONL | binary |
| [`config-loader`](crates/config-loader/README.md) | 所有 binary 共用的設定讀取:路徑、TOML、秘密 | library |

## 快速開始

需要 [rustup](https://rustup.rs/)。repo 用 `rust-toolchain.toml` 鎖定版本,第一次執行 `cargo` 時會自動安裝。

```bash
git clone https://github.com/tommot20077/ai-engineering-lab.git
cd ai-engineering-lab
cargo test --workspace            # 不需要 API key:上游都換成本機的假伺服器

cp .env.example .env              # 填 ANTHROPIC_API_KEY(和 gateway 用的 GATEWAY_TOKEN)
cargo run -p llm --example chat -- "用一句話介紹 Rust"

cp gateway.example.toml gateway.toml
cargo run -p gateway --release    # 用法見 crates/gateway/README.md
```

## 怎麼讀這個 repo

- **`git log` 就是教材。** 每個 commit 只做一件事,依序讀就是從零長出來的過程。
- **每個模組有兩份文件**,放在 `docs/modules/NNN-名稱/`:
  - `design.md`:寫之前,要解決什麼、選了哪個方案、為什麼;
  - `retro.md`:寫完後,學到的 Rust 概念(對照 Java)、架構決策、踩過的坑。
- **程式註解是中文**,寫的是 Rust 特有的東西,並對照 Java。
- 跨模組的決策(D-001、D-002…)記在 [`docs/ROADMAP.md`](docs/ROADMAP.md) 的決策表。

## 品質

| | |
|---|---|
| 測試 | 單元、snapshot(`insta`)、property(`proptest`)、整合(`wiremock`)、E2E(真的執行 binary)、live(手動) |
| Mutation | 每一步都故意把程式改壞,確認有測試會失敗(手寫 + `cargo-mutants`);每週再對整個 workspace 跑一次 |
| 效能 | 只用 `criterion` 的數字談優化;結果寫在各 crate 的 README |
| 安全 | `unsafe` 全面禁止;密鑰包在 `SecretString`,不進 log 與 `Debug`;外部輸入一律驗證、設上限 |
| CI | `fmt`、`clippy -D warnings`(含全部 feature)、`test`、`cargo deny`(供應鏈與授權) |

送出變更前跑 `scripts/check.sh`(跟 CI 一樣的檢查)。clone 之後先跑一次 `scripts/install-hooks.sh`:
commit 前會自動掃描暫存區,擋下 API key 之類的秘密。

## 授權

[MIT](LICENSE)
