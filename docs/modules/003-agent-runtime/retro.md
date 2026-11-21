# 003 Agent Runtime — 回顧(第一部分:agent loop 與內建工具)

> 2026-11-14 完成。設計見 [`design.md`](design.md)(D-041 ~ D-046),結果見
> [`crates/agent/README.md`「第一次真的跑」](../../../crates/agent/README.md)。

## 1. 做出了什麼

- `crates/agent`:工具介面(有型別的 `Tool`、抹除型別的 `ErasedTool`、建立時就檢查的 `Toolbox`)、
  四個沒有副作用的內建工具(`calculator`、`current_time`、`read_file`、`list_dir`)、
  agent loop(`Agent` / `Run`:一步一步拉、引擎自己的護欄、每一種停止原因都明確處理)、軌跡、`agent run` CLI(套 Record)。
- eval:`Input::Agent` / `Output::Agent`、`agent-basics`(18 題)與對照組 `agent-basics-notools`、fixture 檔案。
- llm:修了一個 001 的 bug(沒有參數片段的工具呼叫)。
- 測試:agent 89 個(83 個單元 + 6 個 E2E 真的執行 binary),eval 多了 17 個(13 個單元 + 4 個資料集);每一步都有手寫 mutation,全部抓到或說明了理由。

## 2. 學到的 Rust 概念(依步驟,附 Java 對照)

| 步驟 | 概念 | Java 對照 / 重點 |
|---|---|---|
| 1 | 關聯型別(`type Args`) | 像 `interface Tool<A>`,但一個工具只能有一種 `Args`,呼叫端不必寫型別參數 |
| 1 | trait 方法回傳 `impl Future`,實作寫 `async fn` | compiler 檢查那個 future 是不是 `Send` |
| 1 | blanket impl 做型別抹除(`impl<T: Tool> ErasedTool for T`) | Java 的泛型在執行期被抹除,`List<Tool<?>>` 直接可用;Rust 要自己寫這一層 |
| 1 | serde derive 的 struct 也接受 JSON 陣列 | 解析比 schema 寬鬆;先確認是物件再轉 |
| 2 | `Peekable<Chars>`、`next_if`、複製 iterator 往前看兩個字元 | `Peekable` 是 `Clone`,複製只是幾個指標 |
| 2 | 遞迴的 enum 要 `Box`(`Neg(Box<Expr>)`) | Java 的物件欄位本來就是參考,不用想大小 |
| 2 | stack overflow 是程式中止,不是錯誤 | Java 的 `StackOverflowError` 還能 catch;Rust 只能事先數深度 |
| 2 | 迴圈每一輪先拿走一個字元 | 結構上保證會結束,不靠每個分支都記得往前走 |
| 3 | `Path::components()`、`canonicalize`、`strip_prefix` 比的是「段」 | `/data/root2` 不會被當成在 `/data/root` 底下;中間的 `.` 會被拿掉 |
| 3 | `#[schemars(range(min = 1))]` | schema 從型別推出的最小值(`u64` → 0)不一定是程式的規則 |
| 4 | `buffered` vs `buffer_unordered` | `buffered` 把「好了但還沒交出」的結果算進名額 → head-of-line blocking |
| 4 | `tokio::time::timeout_at`、`start_paused` | 截止時間到就丟掉 future(= 取消請求);測試不必真的等 |
| 4 | `str::floor_char_boundary` | 直接在 byte 位置切字串可能切進中文字中間,`truncate` 會 panic |
| 4 | `Option::inspect`、`fn finish(self)` | 用掉 `self`:呼叫之後不能再 `next()`,Java 只能靠文件 |
| 5 | `type` 別名解 clippy 的 `type_complexity` | |
| 5 | `PoisonError` 裡拿著 `MutexGuard`(不是 `Send`) | `.lock().ok()` 先丟掉它,future 才能跨執行緒 |
| 6 | lifetime 參數(`Agent<'p>` 借用 provider)、covariance | `Run<'a>` 只要一個 lifetime:`Agent<'p>` 可以把 `'p` 縮成 `'a` |
| 6 | 加 enum variant 跟著 compiler 走 | 4 個 `match` + 6 個不再成立的 `let` 被一一列出(D-029 的承諾) |

## 3. 架構決策(詳見 ROADMAP 決策表)

- D-041:分三部分(loop + 工具 / MCP / 沙箱)。這部分 7 步、一天做完,證明切法是對的。
- D-042:單純的迴圈 + 引擎自己的護欄。`Run::next()` 讓呼叫者看得到每一步,卻繞不過護欄;CLI 印進度、寫軌跡都靠它。
- D-043:參數 struct + schemars。兩次抓到「schema 跟程式不一致」:陣列也被接受(第 1 步)、`offset` 的最小值 0(第 3 步)。
  單一來源不等於自動一致,還要看產生出來的 schema(insta snapshot 讓它被看到)。
- D-044:沒有副作用的工具。第一部分完全不需要沙箱;`read_file` 的兩道關卡(字串、檔案系統)是很好的安全題材。
- D-045:CLI 套 Record、eval 不套。Record 是組裝時的決定,agent library 不知道。
- D-046(新):agent **借用** provider(`&'p dyn LlmProvider`)。eval 的執行器手上只有借來的參考,變不成 `Arc`。

## 4. 第一次真的跑:工具 +61,模型 +0

- 給工具:38.9% → 100%(+61.1 [+38.9, +83.3])。全部來自「模型不可能知道」的題目(日期、檔案);沒工具時 Haiku 誠實說不知道。
- 計算題沒工具也全對:Haiku 5.5 的 thinking 算得出這些數字。calculator 的價值要更難的題目才量得到。
- Haiku 對 Sonnet:又是天花板(18/18 對 18/18,A/A 也是 0)。Sonnet 貴 17.7 倍。
- 安全:injection 沒成功;兩個模型都試著讀 `.credentials`、被擋下;Haiku 建議使用者把金鑰複製成一般檔案——工具擋得住,建議擋不住。
- 真的跑抓到 001 的 bug:沒有參數的工具呼叫,串流裡沒有參數片段,組成空字串。

## 5. 學到的教訓

1. **真的跑一次,勝過再多一層單元測試**。空參數的 bug 在 001 寫了完整的 SSE 測試、在 003 寫了 E2E,都沒抓到:
   假的 provider 都是我照「我以為的格式」寫的,每一個工具呼叫都有參數片段。真的 API 第一次就打出來。
   以後新的 provider 或新的事件形狀,要從真的錄下來的串流做測試資料,而不是手寫。
2. **測試資料長得跟實作一樣,就測不出實作的盲點**(同上,也是 002 retro 第二部分的「不對稱資料」)。
3. **mutation 測試也會測到測試環境本身**:cargo-mutants 讓 lexer 不往前走,一直配置記憶體,整台 CI 機器被關掉、什麼結果都沒有。
   修法不是加記憶體上限,而是讓迴圈在結構上一定會結束。CI 的記憶體上限記成待辦。
4. **schema 由型別產生,不等於規則一致**:型別表達不了「最小值 1」「必須是物件」,產生出來的 schema 要看過、鎖住。
5. **「先 stub 再實作」有省略的時候**(第 3~6 步有幾次直接寫實作),要靠 mutation 補回「測試真的會失敗」的證據。
   這次每一步都做了 mutation,但順序反過來時,比較容易寫出「剛好通過」的測試(例如第 5 步的「不等紀錄寫完」只能歸成時序問題)。
6. **eval 題目的難度又一次決定了結論**:002 兩次、003 一次,「兩個模型都全對」。下次寫題前先想「哪一題 Haiku 會錯」,
   而不是「哪一題需要工具」。

## 6. 下一部分要帶走的

- 第二部分(MCP client):`ErasedTool` 就是接點;MCP 工具的 schema 是對方給的,要在執行期驗證(D-043 的另一半)。
  工具名稱來自 MCP server,不可信(`Toolbox` 的錯誤訊息已經用 `{:?}` 印)。
- 第三部分(沙箱):寫檔、跑程式的工具;`ToolContext.call_id` 當 idempotency key;D-040 的 `compiles` 檢查。
- eval:更難的 agent 題(長連續運算、5 輪以上、要規劃順序);system prompt 加「不要建議繞過檔案權限」後再量 `file-secret`。
- 待辦:私有目錄 helper 已經有三份(抽出來);`CallRecord` 記上 run id(006);Mutants (diff) 的記憶體上限。

## 7. 文章大綱

1. 為什麼 agent loop 要自己寫:框架藏起來的東西(停止原因、護欄、工具錯誤怎麼回給模型)正是可靠性所在。
2. 工具介面的兩層:有型別的 `Tool` + 抹除型別的 `ErasedTool`;Rust 的泛型為什麼不能直接放進 `Vec`。
3. 「解析就是驗證」以及它的兩個破洞:serde 接受陣列、schemars 推出最小值 0。
4. 一個 calculator 教會的三件事:Pratt parser、遞迴深度上限(stack overflow 不能 catch)、迴圈要在結構上會結束(mutation 讓 CI 機器當掉的故事)。
5. `read_file` 的兩道關卡:字串的 `components()` 與檔案系統的 `canonicalize`;symlink 指向 `.env` 的那一題。
6. 並行執行工具:`buffered` 的 head-of-line blocking,用暫停的時鐘量出來。
7. 跟 compiler 一起加 enum variant:D-029 預留的位置,在 003 兌現。
8. 第一次真的跑:工具 +61、模型 +0,以及單元測試沒抓到、真的 API 第一次就打出來的 bug。

---

# 第二部分:MCP client

> 2026-11-15 完成(第 1~6 步)。設計見 [`design-part2.md`](design-part2.md)(D-047 ~ D-052),
> 使用說明見 [`crates/mcp/README.md`](../../../crates/mcp/README.md) 與 agent README 的「MCP server」。

## 1. 做出了什麼

- `crates/mcp`:JSON-RPC 2.0(逐條規則解析)、`Transport` trait、stdio 傳輸(子行程、依 id 對回應、取消、stderr 尾巴、關閉)、
  Streamable HTTP 傳輸(POST、JSON 或 SSE、header、`x-mcp-header`)、`McpClient`(`server/discover`、`tools/list`、`tools/call`)、
  測試用的 MCP server(stdio 與 HTTP,可以照旗標故意出錯)、`examples/list_tools.rs`。
- `crates/agent`:`McpTool`(MCP 工具 → `ErasedTool`,jsonschema 驗證參數)、`[[mcp]]` 設定與白名單、CLI 在第一次呼叫模型之前連好所有 server。
- 測試:mcp 118 個(單元 + 整合;client 的整合測試每個都對兩種傳輸各跑一次),agent 多了 31 個(23 個單元 + 8 個 E2E);
  `#[ignore]` 的 live 測試 2 個(對官方參考 server,2026-11-21 跑過、都通過,見 §5 第 5 點)。

## 2. 學到的 Rust 概念(依步驟,附 Java 對照)

| 步驟 | 概念 | Java 對照 / 重點 |
|---|---|---|
| 1 | 不用 derive,先解析成 `Value` 再逐條檢查 | serde 的屬性表達不了「`result` 與 `error` 恰好一個」;`"result": null` 與「沒有 `result`」用 `Option` 分不出來 |
| 1 | `Option::transpose` | `Option<Result<T>>` 換成 `Result<Option<T>>`,才能用 `?` |
| 2 | `tokio::process`、`kill_on_drop` | `ProcessBuilder`;但 Java 的 `Process` 被 GC 時不會殺掉子行程 |
| 2 | 背景 task + `HashMap<id, oneshot::Sender>` | `ConcurrentHashMap<Long, CompletableFuture>`;`oneshot` 只能 complete 一次 |
| 2 | std 的 `Mutex` vs tokio 的 `Mutex` | 拿著鎖要 `.await` 才用 tokio 的;std 的 `MutexGuard` 不是 `Send`,不能跨 `.await` |
| 2 | `mem::replace` 把狀態「搬走」 | Java 沒有搬走的概念,只能複製或共用參考 |
| 2 | `AsyncReadExt::take(limit + 1)` | 對方不送換行也不會一直讀;`&[u8]` 本身就是 `AsyncBufRead`,單元測試不必開 pipe |
| 3 | RAII 的 `Drop` guard 送取消通知 | try-with-resources 的 `close()`;Rust 不管正常回傳、`?`、future 被丟掉都會 drop |
| 3 | `Drop` 裡不能 `.await`:`Handle::try_current()` + `spawn` | |
| 3 | `let … else`、`if let … && 條件`(let chains) | Java 21 的 pattern matching,但可以接 `&&` |
| 4 | trait 的預設方法(`request_with_headers`) | Java 的 `default` 介面方法;stdio 不必實作 |
| 4 | `reqwest` 的 `Policy::none()`、`without_url()` | 不跟 redirect(token 不被帶走)、錯誤訊息不帶 URL |
| 4 | `tests/common/mod.rs` | `tests/` 的每個檔案是獨立的程式;放 `common.rs` 會被當成另一個測試 |
| 4 | `Deref` 委派 | `Conn` 直接呼叫到裡面的 `McpClient` 的方法,像自動產生的 delegate |
| 5 | `Box<dyn ErasedTool>` 混著內建工具與 `McpTool` | 第一部分預留的接點,這次兌現 |
| 5 | 指名呼叫固有方法(`McpClient::call_tool(self, …)`) | 同名的 trait 方法會遞迴呼叫自己 |

## 3. 架構決策(詳見 ROADMAP 決策表)

- D-047 從零寫協定:只做工具時協定真的很小(JSON-RPC 的規則加三個方法),請求與回應的對應、錯誤分類都自己寫過一次。
- D-048 兩種傳輸 + trait:`McpClient` 只看得到 `Box<dyn Transport>`;同一組整合測試對兩種傳輸各跑一次,證明行為一樣。
- D-049 jsonschema:實作時發現它預設的 regex 引擎會回溯(ReDoS)、預設會去抓外部 `$ref`;兩個都要關掉。用現成的 crate 也要讀它的預設值。
- D-050 白名單 + 前綴 + 清空環境變數:全部有 E2E(白名單外的工具連名字都不出現在 prompt、子行程看不到 API key)。
- D-051 自己寫測試 server:能做出真的 server 不會配合的錯誤(半行斷線、繞圈的 cursor、SSE 裡夾請求);刻意不共用 client 的 parser。
- **D-052 協定版本 2026-07-28**:設計時照記憶寫了 `initialize` 握手,實作前查規格才發現最新版已經是無狀態的(沒有 `initialize`、沒有 session、
  多了 `x-mcp-header`)。§4 寫著「實作時先讀當時的規格確認版本字串,不憑記憶」——這一句救了第 3~5 步。

## 4. Mutation 檢查的收穫

- 手寫 mutation 每一步 20~30 個。抓到的測試缺口(都補了):`in_flight()` 只驗了 0、`kill_on_drop` 沒測、連線關閉後的 `notify`、
  非 UTF-8、空行、URL 只有密碼、content type 帶參數、SSE 空資料事件、工具名稱重複、`nextCursor` 不是字串。
- **時間競賽讓 mutation 測不出來**:「不等 stderr 讀完」「關閉時直接 kill」第一次都 MISSED,因為測試 server 結束得太快,有沒有等都一樣。
  修法是讓測試 server 的行為更像真的(stdout 先關、stderr 晚一點寫;收到 EOF 後花 100 ms 收尾)。
- **等價 mutant 是多餘程式碼的訊號**:「有問題的 server 也收」改了也沒差,因為有任何問題整個設定就不會用 → 拿掉那個條件;
  「關閉時沒把原因送給等待中的請求」測不出來,因為有一條「再讀一次原因」的備援 → 拿掉備援。兩次都讓程式變簡單。
- cargo-mutants 在 CI 上第二次讓 runner 被中斷(628 個 mutant,37 分鐘)。把測試 server 排除在外(它是測試的工具);記憶體上限仍是待辦。

## 5. 學到的教訓

1. **規格會變,記憶會過時**:設計文件裡最有價值的一句是「先讀規格,不憑記憶」。下一個協定相關的模組,設計時就先讀規格。
2. **一樣東西用了別人的實作,就要讀它的預設值**:jsonschema 的預設(回溯 regex、抓遠端 `$ref`)在「schema 是不可信輸入」的情境下都是漏洞。
3. **測試 server 要夠「真」才測得出時序**:乾淨俐落的假 server 讓「有沒有等」「有沒有讀 stderr」都看不出差別。
4. **先寫實作再補測試的地方要多做 mutation**:第 4 步 HTTP 與第 5 步設定是先寫實作,mutation 各抓到 3~4 個缺口;先寫測試的部分幾乎沒有。
5. **live 測試晚了幾天才跑,一次就過**:2026-11-15 雲端的權限擋下了執行從 npm 下載的程式;2026-11-21 換了權限模式後跑,
   stdio 與 HTTP 都過,`x-mcp-header` 的 base64 也被參考 server 接受。第一次用 npx 跑時失敗,原因是子行程的環境變數被清空
   (npx 拿不到 proxy 的憑證設定),錯誤訊息裡有 npm 最後印的 stderr —— D-050 與「保留 stderr 尾巴」兩個設計第一次在真的程式上被看見。

## 6. 下一部分要帶走的

- 第三部分(沙箱):寫檔、跑程式的工具;D-040 的 `compiles` 檢查。MCP 的 server 已經是子行程,沙箱可以重用「清空環境變數、kill_on_drop、
  stderr 尾巴、關閉時先等再 kill」這一套。
- 跑 live 測試;有不一致就從真的 server 錄下訊息當測試資料(第一部分的教訓)。
- MCP 的 `instructions`(server 給模型的使用說明)目前沒有放進 prompt:不可信內容,要不要、怎麼放,等真的用到 MCP server 再決定。

## 7. 文章大綱

1. 自己寫 MCP client:協定小到可以一個週末寫完,難的是「對方不可信」。
2. JSON-RPC 的對應:一個背景 task、一張 `id → oneshot` 的表,以及 future 被丟掉時要做的事(RAII 送取消)。
3. 子行程不是黑盒子:清空環境變數、持續讀 stderr(不然會卡住)、關閉時先等再殺。
4. 規格改版的那一天:設計照記憶寫了 `initialize`,實作前讀規格發現已經沒有了。
5. 用現成的 JSON Schema 驗證器,也要讀它的預設值:ReDoS 與外部 `$ref`。
6. 同一組測試跑兩種傳輸,以及讓 mutation 測得出時序的「更真的」測試 server。
