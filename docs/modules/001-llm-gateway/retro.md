# 001 LLM Gateway — 回顧(第一部分:`crates/llm`)

> 2026-10-24 ~ 2026-10-26。第二部分(decorator + `crates/gateway`)的回顧在[後半](#第二部分decorator--cratesgateway)。

## 1. 做出了什麼

一個與 provider 無關、以串流為核心的 LLM client,接上真的 Claude,兩輪對話與串流範例都成功。

| | |
|---|---|
| Commit | 9 步,每一步一個 commit,`git log` 就是教材 |
| 程式碼 | `src/` 約 3,200 行(含 inline 單元測試),`tests/`、`benches/`、`examples/` 約 600 行 |
| 測試 | 78 個:單元、property、snapshot、整合(wiremock)、live(真的 API) |
| 文件 : 程式 | 約 1 : 7(Go 專案是 17 : 1,反過來) |
| 效能 | SSE 解析 428 MiB/s;一則幾秒的回答只要 0.3 ms 解析,不需要優化 |

## 2. 學到的 Rust 概念(依出現順序,附 Java 對照)

| 步驟 | 概念 | Java 對照 |
|---|---|---|
| 1 | Cargo workspace、`[workspace.lints]`、`forbid` vs `deny` | Maven 多模組 + 集中的 lint 設定 |
| 2 | `struct` / 帶資料的 `enum`、`derive`、serde 的 `tag` / `rename_all` / `transparent` | sealed interface + record、Lombok、Jackson 註解 |
| 2 | `Option`(沒有 null)、newtype、`impl Trait for Type` | `Optional`、包一層的 value object、`implements` |
| 2 | 手寫 `Debug` 遮罩內容;`match` 窮舉讓「新種類必須決定怎麼遮罩」 | 覆寫 `toString()`,但 Java 不會逼你補新的分支 |
| 3 | `Result` + `?` + `impl From`;`thiserror`;`Box` 縮小 enum | checked exception + 每處手寫 try/catch 包裝 |
| 3 | `String::len()` 是 bytes;切字串要找字元邊界;第一次遇到 lifetime(省略寫法) | Java 的 `String` 是 UTF-16,`substring` 會複製 |
| 4 | **所有權**:傳值 = 交出去;`&` 借看;`&mut` 借改;`finish(self)` 用完就消失 | Java 永遠傳參考,「用完不能再用」只能執行期檢查 |
| 5 | `&[u8]`、`mem::take` / `mem::replace`(避開 borrow 衝突又不複製)、增量解析 | 自己寫 `BufferedReader.readLine()` |
| 6 | 明確的 lifetime `'a`:借用整段對話來序列化,一個字都不複製 | 沒有對應:GC 讓你不必想這件事 |
| 6 | `pub(super)`、`#[serde(flatten)]`、`#[serde(other)]`、`RawValue`、`?` 用在 `Option` 上 | package-private、`Optional.map().map()` |
| 7 | `async` / `.await`(Future 是懶的)、`BoxStream` / `dyn` / `Send + Sync`、`try_stream!` | `CompletableFuture`(但會立刻執行)、介面 + 執行緒安全只靠文件 |
| 8 | `Box<dyn Error>`、`#[tokio::main]`、stdout 與 stderr 分工 | `main(...) throws Exception` |
| 9 | criterion、`black_box`、`Throughput` | JMH |

## 3. 架構決策(詳見 ROADMAP 決策表)

- **D-006** library 先、server 後 → `crates/llm` 可以被 agent、eval 直接引用,不必經過 HTTP。
- **D-009 / D-010** streaming-first + 中立事件(參考 Vercel AI SDK)→ 只有一條資料路徑;
  opaque `meta` 保住 thinking 簽章,live test 證明兩輪對話原樣回送可行。
- **D-011** 對外格式與內部分離 → 之後在 gateway 額外開 OpenAI 相容端點,`llm` 不動。
- **D-016** 常數跟著用途走、`Limits` 從 constructor 注入、library 不讀檔案也不讀環境變數。
- 實作時補上的:`Interrupted` 帶 `cause`;`Content` 每種都帶 `meta`;未知事件類型忽略、未知內容類型拒絕。

## 4. 學到的教訓

1. **測試「用錯誤的理由通過」是這個專案最常見的漏洞,共 4 個測試(第 4 步 3 個、第 7 步 1 個)。** 協定錯誤的規則被關掉後,錯誤改在
   更後面的 `finish()` 才出現,測試只檢查「有沒有錯」,就照樣通過。解法:測試要指名「哪一個步驟被拒絕」,
   例如 `rejects_last`;非 SSE 的測試改送「格式完全正確、只有 content-type 不對」的 body。
   **只有 mutation 檢查抓得到這種問題**,光看測試全綠看不出來。
2. **Property test 抓到我想不到的情況。** 「一行空的 `data:`」只有 proptest 自動產生的輸入發現。
3. **Snapshot 一定要人工審查。** 第一次的 snapshot 把工具參數記成 `$serde_json::private::RawValue`,
   是 insta 自己的序列化方式造成的假象;不審查就會把錯的內容當成正確答案存下來。
4. **測試輔助程式也會有 bug。** 假 provider 把所有錯誤都變成 `Transport`、wiremock 的 `set_body_string`
   偷偷把 content-type 改成 `text/plain`。兩次都是產品程式正確地拒絕了錯的輸入。
5. **Benchmark 告訴我們「不要做什麼」。** 逐 byte 的解析器已經夠快,沒有為了感覺去寫 SIMD。
6. **流程:** 每步自動 commit + push(D-015)之後,節奏明顯順很多;使用者只在設計分岔時參與。

## 5. 下一部分要帶走的

- 把「指名哪一步被拒絕」當成寫拒絕測試的預設寫法。
- 考慮在 CI 加 `cargo-mutants`,讓 mutation 檢查不只靠手動。
- Anthropic 事件的 JSON 解析還沒量過(不是公開 API),gateway 的端到端 benchmark 要涵蓋。
- gateway 要決定:模型別名、呼叫紀錄的格式與保存、OpenAI 相容端點的時機。

## 6. 文章大綱

**標題:〈從 Java 到 Rust:用 TDD 從零寫一個 streaming-first 的 LLM client〉**

1. 為什麼不用現成的 SDK 或框架(學習密度、Rust 沒有官方 SDK)
2. 中立格式怎麼設計:參考 Vercel AI SDK,以及「保管箱」opaque metadata
3. 型別系統當安全網:手寫 `Debug` 遮罩、`SecretString`、`match` 窮舉
4. 所有權對一個 Java 開發者的意義:`finish(self)`、lifetime 讓序列化零複製
5. 串流的難題:SSE 被切成碎片、中途斷線的語意(可以重試 vs 不能重試)
6. 測試全綠不代表測試是對的:mutation 檢查抓到的 4 個漏洞
7. 先量再優化:428 MiB/s 告訴我們不要寫 SIMD
8. 結語:文件與程式比 1:7,以及下一步的 gateway

---

# 第二部分:decorator + `crates/gateway`

> 2026-10-30 ~ 2026-11-01。設計見 [`design-part2.md`](design-part2.md),決策 D-017 ~ D-028。

## 1. 做出了什麼

一個每天可以用的本機 LLM 服務:別名、重試、Host 與 token 檢查、上限、優雅關機,
而且**每一次呼叫都完整記錄下來**,這是階段 2 資料飛輪的原料(D-014)。

```
你的程式 ──HTTP──→ gateway ──→ Record → Retry → Alias → AnthropicClient ──→ Claude
```

| | |
|---|---|
| Commit | 第 10~18 步,14 個程式 commit(加上 10 個設計文件 commit) |
| 程式碼 | `llm/middleware` 約 1,900 行、`config-loader` 約 400 行、`gateway` 約 2,300 行(都含 inline 測試);E2E 與 benchmark 約 800 行 |
| 測試 | 整個 workspace 189 個(第二部分新增約 110 個),另有 2 個手動執行的 live test |
| 文件 : 程式 | 約 1 : 8.5 |
| Mutation | cargo-mutants:gateway 從 15 個漏掉降到 2 個(一個等價、一個是常數,都說明過理由) |
| 效能 | 一則 1000 個 delta 的長回答,SSE 解析 + 翻譯 + 累加全程約 0.5 ms,不需要優化 |

## 2. 學到的 Rust 概念(附 Java 對照)

| 步驟 | 概念 | Java 對照 |
|---|---|---|
| 10 | 泛型 decorator `Alias<P>`、monomorphization、blanket impl(`impl<T: LlmProvider + ?Sized> for Arc<T>`)、`?Sized`、extension trait | 介面 + 包裝類別;`default` 方法;但 Java 的泛型會被擦除,Rust 會替每種組合各編譯一份 |
| 10 | 泛型(編譯期組合)與 `Arc<dyn Trait>`(執行期才知道的分岔)各用在哪裡 | 全部都是介面 + 動態分派 |
| 11 | `tokio::time::sleep` + `start_paused`(暫停時間)、labeled loop、飽和運算(debug 模式溢位會 panic) | `ScheduledExecutorService`、可注入的 `Clock` |
| 12 | `Drop` 保證「剛好送出一次」、`Option::take`、`Instant`、UUID v7 | try-finally / `Cleaner`,但 `Drop` 的時機是確定的 |
| 13 | OS 執行緒 + 有上限的 `sync_channel`(`try_send` 不阻塞)、`AtomicU64`、`OpenOptionsExt::mode`、`sync_data` | 單執行緒 `ExecutorService` + `ArrayBlockingQueue.offer`、`AtomicLong`、`FileChannel.force` |
| 14 | `Path` / `PathBuf`(像 `&str` / `String`)、`load<T: DeserializeOwned>` | `Path`、Jackson 的 `readValue(file, Class<T>)` |
| 15 | 同一個 crate 裡的 lib + binary、`anyhow` vs `thiserror`、`deny_unknown_fields`、struct update `..`、`RawConfig → Settings` 兩層型別 | `@ConfigurationProperties` + `@Validated`,但「沒檢查過的設定」在型別上拿不到後面去用 |
| 16 | axum extractor、`IntoResponse`、`Result<Json<T>, JsonRejection>` 當參數、`Sse`、`stream::once(..).chain(..)` | `@RequestBody`、`@ControllerAdvice`、`SseEmitter` |
| 17 | `middleware::from_fn_with_state` 與 layer 的順序、`Semaphore::try_acquire_owned`(permit 被 drop 就歸還,跟著串流走)、`subtle` 固定時間比較、`watch` channel、`select!`、`spawn_blocking`、Unix 信號 | Servlet `Filter`、`Semaphore.tryAcquire` + finally、`MessageDigest.isEqual`、`volatile boolean` |
| 18 | `env!("CARGO_BIN_EXE_gateway")`、`tokio::process` + `kill_on_drop`、cargo feature + `#[cfg(feature)]`、`required-features`、從私有模組 re-export | `@SpringBootTest(RANDOM_PORT)`、`ProcessBuilder`、Maven profile |

## 3. 架構決策(詳見 ROADMAP 決策表)

- **D-017 / D-021** 紀錄保留完整內容,個資遮蔽留到匯出成訓練資料時才做;放專案外、700/600;
  寫檔在獨立執行緒,佇列滿了丟掉並計數,**不拖慢請求**。
- **D-018 ~ D-020** middleware 就是 `LlmProvider` 的 decorator,放在 `crates/llm`(agent、eval 不經過 HTTP 也能用);
  順序 `Record [ Retry [ Alias [ client ] ] ]`:Record 看到呼叫端的原始請求,重試只記一筆。
- **D-022** 只在「還沒輸出任何內容之前」重試,開始事件先扣住;full jitter,`retry-after` 超過上限就不等。
- **D-023 / D-024** 原生 HTTP API、未知別名回 400;設定一次列出全部問題;對外 bind 要「寫兩次」(`allow_remote` + `allowed_hosts`)。
- **D-025** `/health` 公開、`config-loader` 獨立成小 crate;library 不讀環境變數(D-016 延續)。
- **D-026 ~ D-028** 流程工具:`lab-step` skill、git hook 掃描私有資料、cargo-mutants、量內部函式的 feature 後門。
- 實作時補上的:串流端點**先等第一個項目再決定狀態碼**;permit 跟著 SSE 串流走;上游 401 回 502 而不是 401;
  port 0 時允許的 Host 要用實際的 port(`Settings::bound_to`)。

## 4. 學到的教訓

1. **「測試用錯誤的理由通過」又出現了 5 次,而且是新的形態。** 第一部分的教訓(指名哪一步拒絕)照用了,還是不夠:
   - Alias 的拒絕測試其實是 `validate()` 先擋下的(請求本身就不合法);
   - 只比對「訊息裡有 `allow_remote`」,另一條檢查的訊息也有這個字;
   - Record 的違規測試,違規之後沒有 `Finish`,忽略違規的實作也一樣失敗;
   - 「Host 錯 + token 對」兩種檢查順序都回 403,測不出順序;
   - 「等待時間不超過上限」對「永遠等上限」也成立,測不出隨機。
   每一種都寫進了 `lab-step` 的檢查清單,變成之後每一步的預設檢查。**只有 mutation 抓得到這類問題。**
2. **兩種 mutation 檢查互補。** 手寫的擅長語意(layer 順序、permit 跟不跟著串流);
   cargo-mutants 擅長機械式的盲點:`>` 換 `>=` 的邊界、錯誤訊息的格式、範例檔與常數不一致、被忽略的 `base_url`。
   第一次跑 gateway 漏掉 15 個,依「A 補測試 / B 交給 E2E / C 說明理由」分類後降到 2 個。
3. **E2E 是照著 B 類 mutant 設計的,而且順便抓到一個真的 bug。** `main`、`serve`、信號處理、`base_url` 都是單元測試碰不到的;
   為了「真的執行 binary、port 寫 0」,才發現允許的 Host 會變成 `127.0.0.1:0`,每個請求都 403。
   設定裡「依賴執行期事實」的值(實際的 port),要在事實確定之後才算。
4. **有些保證測不出來,要承認。** 「關機時把佇列裡的紀錄寫完 + `sync_data`」只在很窄的競爭窗口或斷電時才看得出差別;
   把 `shutdown()` 換成什麼都不做,所有測試照樣通過。這類保證靠設計文件與程式審查撐住,在導讀裡誠實寫出來。
5. **安全工具本身也要測。** 私有資料掃描的 git hook 第一版用了管線(`cmd | scan_text`),函式在子 shell 裡執行,
   找到的東西出了子 shell 就消失,**什麼都沒擋下來**;測試 commit 因此帶進了不該有的內容,幸好還沒 push,在本機清掉。
   之後又發現掃描腳本自己含有 key 的格式,被自己擋下。教訓:先直接測腳本的 exit code,再用真的 commit 測。
6. **平台與版本的坑**:macOS 沒有 `timeout` 指令(mutation 腳本誤判成編譯失敗);Rust 2024 的 `set_var` 是 unsafe
   (把讀環境變數拆到外層,測試只測純函式);`Path` 的 `==` 會忽略 `.`;`reqwest` 不轉出 `url::Host`(改成直接依賴 `url`)。
7. **Benchmark 的數字一天可以差 15%。** 同一台筆電,SSE 解析 10-26 是 307 µs、10-31 是 266 µs。只比較同一次執行裡的數字。
8. **流程:把教訓變成工具。** `lab-step` skill 把每個踩過的坑寫成檢查清單;`check.sh`、`mutate.py`、git hook 讓檢查不靠記憶。
   中途暫停、對話被壓縮後,靠 ROADMAP 的「目前位置」接回來,沒有遺失進度。

## 5. 下一部分要帶走的

- **cargo-mutants 進 CI**:每週排程跑一次、只回報不擋;先用 `.cargo/mutants.toml` 排除已說明的等價 mutant(D-027)。
- **repo 根目錄還沒有 README。** 這是公開專案,別人 clone 下來第一眼看的就是它(D-013)。
- 紀錄的保留期限與壓縮(006)、執行中可替換的路由表(011)、OpenAI 相容端點(D-011)都還沒做,已記在 ROADMAP。
- 寫拒絕類測試時,照 `lab-step` 的清單逐條檢查;每一步都跑 cargo-mutants。

## 6. 文章大綱

**標題:〈用 Rust 寫一個會記帳的 LLM gateway:decorator、優雅關機,以及「測試到底在測什麼」〉**

1. 為什麼 gateway 從第一天就要記錄每次呼叫(資料飛輪的原料)
2. decorator 鏈:泛型 vs `dyn`、blanket impl,以及為什麼順序是 Record → Retry → Alias
3. 串流時的重試:只在第一個字之前重試、扣住開始事件
4. 「剛好記一筆」:用 `Drop` 對付取消、逾時與 panic;背景寫檔執行緒與有上限的佇列
5. 本機服務也要防:DNS rebinding、固定時間比較 token、permit 跟著串流走
6. 優雅關機的細節:`watch` + `select!`、為什麼紀錄要在 `spawn_blocking` 裡寫完
7. 測試全綠也不夠:5 種「用錯理由通過」、cargo-mutants 的 A / B / C 分類、E2E 怎麼照著 mutant 設計
8. 測不出來的保證,以及一個沒擋下任何東西的安全 hook
9. 結語:0.5 ms 的處理時間,和下一步的 Eval

