# 001 LLM Gateway — 回顧(第一部分:`crates/llm`)

> 2026-10-24 ~ 2026-10-26。第二部分 `crates/gateway` 完成後再補上第二份回顧。

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
