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
| **1. 個人助理** | 自己每天用的助理:介面做薄,底層做深(自寫檢索引擎、agent runtime、guardrails、MCP)。Gateway 從第一天就記錄每次呼叫 | 實際每天使用、連續數週;Eval 測試集持續累積;呼叫紀錄持續累積(只存本機、權限 600;個資在匯出時遮蔽,D-017) |
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
- **下一步**:設計 001 第二部分:`crates/llm` 的 decorator(D-018)+ `crates/gateway` 外殼。
  設計已定案(D-017~D-025,`docs/modules/001-llm-gateway/design-part2.md`)。
  第 10~14 步完成(`Alias`、`Retry`、`Record`、`JsonlWriter`、`crates/config-loader`);下一步第 15 步 `crates/gateway` 骨架。
  第一部分回顧見 `docs/modules/001-llm-gateway/retro.md`
- **最後更新**:2026-10-31

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
| D-017 | 呼叫紀錄(給飛輪的資料)寫入時保留完整內容,只存本機、權限 600;個資遮蔽在匯出成訓練資料時才做;營運 log 與資料紀錄分開存放,營運 log 不含內容 | 寫入時遮蔽一旦規則寫錯就無法復原,被遮掉的內容也無法訓練;這是個人資料、存在自己的電腦上,用存取控制保護 | 2026-10-30 |
| D-018 | Retry、Record、Alias 等 middleware 寫成 `LlmProvider` 的 decorator(每層都實作 `LlmProvider`、一層包一層),放在 `crates/llm` 裡用 module 分開;`crates/gateway` 只是 HTTP 外殼(HTTP ↔ `LlmProvider`、token 驗證、讀設定檔) | 直接引用 library 的 Rust 呼叫者也經過同一組 decorator,紀錄不會有缺口;之後 010 路由、011 灰度切換也只是再包一層 | 2026-10-31 |
| D-019 | Decorator 用泛型 struct(`Retry<P>`),再加 blanket impl 讓 `Arc<T: LlmProvider + ?Sized>` 也是 provider;另提供 `with_xxx` extension trait 方便組裝 | 測試可用具體型別;gateway 可依設定檔在執行期組裝 `Arc<dyn LlmProvider>`;與 axum/tower 的 middleware 模式一致 | 2026-10-31 |
| D-020 | 包裝順序 `Record [ Retry [ Alias [ client ] ] ]`:Record 在最外層,一次呼叫一筆紀錄,同時記請求的別名與 `Response.model`(真正回答的模型,含伺服器端 fallback);重試的每次嘗試由 Retry 寫進營運 log(不含內容) | 飛輪要的是「問題 → 最終答案」配對;使用者感受的總延遲才是真實延遲;失敗嘗試屬於除錯資訊,依 D-017 與資料紀錄分開 | 2026-10-31 |
| D-021 | 呼叫紀錄:(1) 一筆含 schema_version、UUID v7 id、別名與 served model、完整 request/response、outcome(ok/error/interrupted/cancelled)、usage、首字延遲與總延遲;失敗與取消也記(2) JSONL、每天(UTC)一個檔;預設放專案外(macOS `~/Library/Application Support/ai-engineering-lab/records`),由 gateway.toml 設定、啟動時印出位置;目錄 700、檔案建立時即 600;`.gitignore` 仍加 `/records/`(3) 啟動時檢查目錄可寫,不行就拒絕啟動;執行中寫入失敗不影響回答,但記 ERROR 並累計遺失數;一筆一次寫入整行,讀取端跳過殘行(4) Record 邊抄邊傳不扣事件,結束(含 Drop 取消)時把紀錄丟進有上限的 channel,由單一背景 writer 寫檔;佇列滿則丟棄並報錯;關機前清空佇列 | 飛輪要完整配對,過濾容易但沒記就找不回;JSONL 寫入與整批讀取最簡單,查詢需求到 006 再匯入;專案外避免 `git clean -fdx` 誤刪與工具誤讀;記錄是附帶工作,不能拖垮回答也不能悄悄失敗 | 2026-10-31 |
| D-022 | Retry 預設:最多 3 次嘗試;指數退避 + full jitter(第一次上限 1 秒、每次加倍、最長 10 秒),測試用 tokio 暫停時間;有 `retry-after` 就照它,但超過最長上限就不等、直接回傳錯誤;數值放 `RetryPolicy::default()`,可由 gateway.toml 覆寫 | 伺服器端 fallback 已處理多數過載;越等越久給對方喘息、隨機避免同時重試;對話情境不宜久等,把長時間不可用交給上層(之後 010 可改路由) | 2026-10-31 |
| D-023 | gateway 原生 HTTP API:(1) `POST /v1/chat` 回完整 `Response` JSON、`POST /v1/chat/stream` 回 SSE,body 就是 `Request`(2) SSE 每則 `event: <type>` + `data: <StreamEvent JSON>`,meta 原樣傳出;中途出錯送 `error` 事件後關閉;沒有 finish/error 就斷線視為中斷;每 15 秒送 `: keep-alive`;不送 `[DONE]`(3) 錯誤轉成對呼叫者有意義的狀態碼(上游 401/403 → 502、429 → 429 附 Retry-After、逾時 → 504、其餘上游 → 502);統一 body `{"error":{kind,message,retryable}}`,上游 body 只在 400 時附上;串流端點等第一個項目再決定狀態碼;`Interrupted` 附 partial(4) `GET /health`(含遺失紀錄數)、`GET /v1/models`(別名清單);body 上限 10 MiB、同時上游呼叫 16(超過回 429 gateway_busy)、非串流總時間 10 分鐘;優雅關機 30 秒並清空紀錄佇列;未知別名回 400(Alias 嚴格模式由 constructor 決定) | 每個端點只有一種回應格式,body 直接用現成型別;沿用自家 SseParser/StreamEvent 可 dogfooding;狀態碼要告訴呼叫者該怎麼辦;別名表是成本與換模型的控制點 | 2026-10-31 |
| D-024 | gateway 強制使用 token:隨機固定 token(非 JWT)放 `.env` 的 `GATEWAY_TOKEN`、包 `SecretString`、固定時間比較(`subtle`)、沒設定就拒絕啟動;另檢查 `Host` header 擋 DNS rebinding、不開 CORS。設定檔用 TOML:`gateway.example.toml` 進 git、`gateway.toml` 不進;秘密只放 `.env`;`deny_unknown_fields` 與數值、目錄、bind 位址全部在啟動時檢查,非本機 bind 需明確 `allow_remote = true`;別名先放設定檔(改動頻率低),執行中可替換的表留到 011 | 本機連得到 ≠ 只有本人(瀏覽器、其他程式);單一使用者用 JWT 只增加金鑰與過期管理的風險;TOML 型別明確、是 Rust 慣例,YAML 有隱式型別陷阱且主流 serde 函式庫已封存;設定錯誤要在啟動時擋下 | 2026-10-31 |
| D-025 | `/health` 公開(不檢查 token 與 Host、只回 ok),內部資訊移到需要 token 的 `/v1/status`;新增 `crates/config-loader` 給所有 binary 共用:只展開開頭 `~/`、不展開 `$VAR`、相對路徑以設定檔所在目錄為準、預設資料目錄、讀 TOML(錯誤含行號)、讀秘密環境變數成 `SecretString`;library 不依賴它 | 服務檢查帶不了 token,而 `/health` 不洩漏資訊也不花錢;002、005 也會讀設定檔,規則(特別是相對路徑)要一致 | 2026-10-31 |

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
| 011 | 灰度切換與自動回滾(依 Eval 統計證據決定;**需要執行中可替換的別名/路由表**,見待辦) | Rust 服務 | — |

**比較基準要包含 Haiku**:自己蒸餾的學徒模型,在品質與成本上必須贏過「直接把簡單請求交給
`claude-haiku-5-5`」(比 Sonnet 便宜約 20 倍);贏不過的話,飛輪就沒有意義,直接路由給 Haiku 即可。

### 階段 3:RL

| # | 模組 | 層 | 狀態 |
|---|---|---|---|
| 012 | RL 環境與判分器(高速執行任務、驗證答案) | Rust 服務 | — |
| 013 | GRPO 訓練(先 0.5~1.5B 跑通,再放大) | Python 模型 | — |
| 014 | RL 學徒 vs 模仿學徒的比較與上線 | 全層 | — |

**001 的提醒**:`crates/gateway` 那部分要從第一天就記錄每次呼叫(輸入、輸出、用量、延遲),
只存本機、檔案權限 600,這是階段 2 的原料。紀錄保留完整內容,個資遮蔽在 007 匯出成訓練資料時才做(D-017);
營運 log(`tracing`)與資料紀錄分開,營運 log 永遠不含內容(design.md §7)。

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
- **011 灰度切換(重要,使用者要求記下)**:011 要在**執行中**依 Eval 結果改變流量分配
  (例如把 10% 的 `cheap` 切給小模型,品質下降自動切回),不能靠改設定檔重啟。到 011 時設計
  「執行中可替換的別名/路由表」與它的存放位置(檔案熱重載、資料庫或其他),以及變更的稽核紀錄。
  001 已預留:`Alias` 只從 constructor 拿到表、不知道表從哪裡來(D-024),換掉提供者即可,`Alias` 不用改。
- **呼叫紀錄的保留期限與壓縮(006 一起設計)**:目前只依 UTC 日期切檔,不依大小切(每天約數 MB)。
  到 006 時決定舊檔多久壓縮、多久刪除,以及要不要匯入 DuckDB / SQLite 查詢。
- **模型別名**:已決定(D-023、D-024),別名放 `gateway.toml`,未知別名回 400。
  `LLM_MODEL`(範例)、`LIVE_MODEL`(live test)兩個環境變數維持給 `crates/llm` 的範例與測試用。

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
