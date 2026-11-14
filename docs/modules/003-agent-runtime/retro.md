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
