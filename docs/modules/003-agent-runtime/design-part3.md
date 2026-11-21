# 003 Agent Runtime 第三部分 — 沙箱與有副作用的工具

> 狀態:**已定案(2026-11-21)**。草案(2026-11-15)由 Claude 先選建議方案,使用者逐一確認;P3-4 改成「旗標 + 每次核准」兩個都要,
> 核准用狀態機並能存檔接續;P3-5 加上 `edit_file`。決策記在 ROADMAP 決策表(D-053 ~ D-058)。
> 第一部分見 [`design.md`](design.md),第二部分見 [`design-part2.md`](design-part2.md)。

## 1. 要解決什麼

讓 agent 做有副作用的事,而且是對**不可信的輸入**做:

- **寫檔、改檔**:個人助理(005)要能寫筆記、改草稿。路徑與內容是模型給的。
- **跑程式**:驗證一段程式碼能不能編譯、執行結果是什麼。程式碼是模型寫的。
- **eval 的 `compiles` 檢查**(D-040):回答裡的 Rust 程式碼到底能不能編譯。

**威脅模型**:模型可能被 prompt injection 操縱(工具結果、MCP 的說明、檔案內容都是入口),或單純寫出惡意、失控的程式碼。

| 威脅 | 例子 |
|---|---|
| 讀寫根目錄以外的檔案 | `../../.ssh/id_ed25519`、指向外面的 symlink、檢查完到寫入前被換成 symlink(TOCTOU) |
| 洩漏資料 | 程式碼把讀到的東西送到外部網路 |
| 讀秘密 | 環境變數裡的 API key |
| 耗盡資源 | fork bomb、無窮迴圈、塞爆磁碟或記憶體、輸出幾 GB |
| 留下東西 | 背景行程、暫存檔 |
| 未經同意的修改 | 模型被騙去覆寫重要的檔案 |

**編譯也是執行**:`include_str!("/Users/me/.ssh/id_ed25519")` 在**編譯時**讀檔。編譯不可信的程式碼也需要隔離。

## 2. 決策(2026-11-21 定案)

| # | 決策 | 理由 |
|---|---|---|
| P3-1 | 範圍:`write_file`、`edit_file`、在沙箱裡編譯並執行單檔 Rust 的 `run_rust`、eval 的 `compiles` 檢查 | 三個目標都涵蓋;「跑程式」限定單檔 Rust,攻擊面可控。通用 shell 不做 |
| P3-2 | 隔離用**容器**(Docker 或 Podman):`--network none`、唯讀根目錄、只有一個 tmpfs 工作目錄、非 root(nobody)、`--cap-drop ALL`、`no-new-privileges`、記憶體 / CPU / 行程數 / 時間上限、`--rm` | Mac 與 Linux CI 行為一樣;2026-11-21 在雲端實測過每一項(身分、網路、唯讀、capability) |
| P3-3 | 檔案工具改用 **`cap-std`**:從根目錄的 handle 一段一段往下開、拒絕 symlink;`read_file`、`list_dir` 一起改 | 「先檢查字串、再開檔」中間有空隙(TOCTOU),寫入不能復原;三個工具同一套規則 |
| P3-4 | 有副作用的工具:`--allow-write`、`--allow-run` 打開才註冊(預設關);打開之後**每次呼叫都要核准**,`--yes` 才不問;eval 一律不問 | 兩層:要不要給模型這個能力、每次要不要過目 |
| P3-4b | 核准用**狀態機**:`Run::next()` 回傳「需要核准」,呼叫端回答後接續 | 核准可以晚一點才給,不必讓 loop 卡在一個 `.await` 上 |
| P3-4d | 暫停的狀態可以**存檔**,之後 `agent resume <run-id>` 接續 | 狀態機的好處要用得到:可以過幾小時再核准、中間程式重開也沒關係 |
| P3-5 | `write_file`(新建或整份覆寫)+ `edit_file`(`old` 必須在檔案裡**剛好出現一次**,換成 `new`);都是暫存檔 + rename;核准時顯示具體變更 | 更新是常見需求;整份重寫貴又容易誤改;「剛好一次」擋掉改錯位置 |
| P3-6 | `compiles` 只編譯:取回答裡最後一個 ` ```rust ` 區塊,沙箱裡 `rustc --edition 2024`,結束碼 0 才過 | D-040 要的;「執行比對輸出」之後用同一個沙箱再加 |
| P3-7 | image 用官方 `rust:<版本>-slim`,digest 鎖定,版本跟 `rust-toolchain.toml` 一致 | 不必維護 Dockerfile;digest 鎖住,被換掉會失敗 |

**小決定**:沙箱預設上限 —— 編譯 20 秒、執行 5 秒、記憶體 512 MiB、行程數 64、輸出 64 KiB、tmpfs 64 MiB、程式碼 64 KiB;
可以在 `agent.toml` 調整、不能關掉(同第一部分的護欄)。新的 crate `crates/sandbox`(agent 與 eval 都用,不知道它們存在)。
容器指令可設定為 `docker` 或 `podman`(兩者的 CLI 參數相容),預設 `docker`。

## 3. 狀態機與接續(P3-4b、P3-4d)

```text
Run::next() ─→ Step(跟現在一樣:一輪模型回答 + 工具結果)
            └→ NeedsApproval { pending: [工具呼叫 + 要做什麼的說明] }   ← 有副作用的工具,執行前停下來
呼叫端 ─→ run.resolve(決定)        每個 pending 各自允許或拒絕;拒絕的回報給模型「使用者拒絕」
       └→ run.suspend() → 可序列化的 Suspended(對話、用量、計數、pending)→ 存檔
agent resume <run-id> ─→ 讀檔還原 Run → 問核准 → 接著跑
```

- 工具自己說明「要做什麼」(`ErasedTool` 加一個有預設實作的方法:沒有副作用的工具回 `None`,不需要核准)。
  `edit_file` 顯示改前、改後的那一段;`write_file` 顯示新建或覆寫、大小;`run_rust` 顯示程式碼。
- 同一輪裡沒有副作用的工具照常執行,有副作用的一起等核准(結果的順序仍照模型呼叫的順序)。
- 存檔放在軌跡旁邊的私有目錄(700 / 600),內容等同軌跡(D-017)。接續時重新檢查設定與工具清單,跟暫停時不一樣就拒絕接續。
- 護欄:等待核准的時間**不算**進總時間(`deadline_secs` 只算真的在跑的時間);輪數、工具呼叫、token 照算。

## 4. 結構

```text
crates/sandbox/            沙箱(不知道 agent、eval 存在)
  src/lib.rs               Sandbox trait、Limits、Output(結束碼、stdout / stderr 的開頭、是否逾時)
  src/container.rs         ContainerSandbox:組 `docker run` 的參數(單元測試鎖住每一個)、執行、收結果
  tests/escape.rs          逃逸測試(真的容器)
crates/agent/src/
  run.rs                   NeedsApproval、resolve、suspend / resume
  tools/files.rs           Root 改用 cap-std;write_file、edit_file
  tools/rust.rs            run_rust
crates/eval/src/           Check::Compiles
```

## 5. 實作順序

會讓某個 commit 處於不安全狀態的順序要避開:寫檔工具在核准機制之後才出現。

| # | 內容 | 主要 Rust 概念 |
|---|---|---|
| 1 | `crates/sandbox`:trait、上限、`ContainerSandbox`、逃逸測試 | `std::process::Command`、逾時與確實收掉容器 |
| 2 | 狀態機:`NeedsApproval`、`resolve`;工具的「要做什麼」說明 | enum 狀態、`Step` 加 variant 跟著 compiler 走 |
| 3 | `Root` 改用 cap-std;`write_file`、`edit_file`;`--allow-write`、核准提示、`--yes` | capability-based API(目錄 handle 取代路徑字串)、暫存檔 + rename |
| 4 | 存檔與接續:`suspend`、`agent resume <run-id>` | serde 序列化執行狀態、版本欄位 |
| 5 | `run_rust`、`--allow-run` | 把不可信的程式碼交給 trait object |
| 6 | eval 的 `compiles` 檢查;資料集的題目改寫 | 在 `Check` enum 加 variant |
| 7 | E2E、README、retro | — |

## 6. 測試

- 單元:`docker run` 的參數(一個一個鎖住)、程式碼區塊的擷取、路徑規則、`edit_file` 的「剛好一次」、狀態機的每一種轉換、序列化往返。
- 整合(要真的容器):逃逸測試 —— 連網路、讀 `/etc`、寫根目錄、fork bomb、無窮迴圈、大量輸出、`include_str!` 讀主機檔案、
  留下背景行程,每一項都要被擋下或被上限停住。設 `SANDBOX_DOCKER=1` 時才跑;設了卻找不到 docker 就**失敗**(不是略過),CI 設它。
- E2E:agent 用 `write_file` 時停下來等核准、拒絕、`--yes`;暫停後 `agent resume` 接續;`run_rust` 回答一題要算的;eval 跑 `compiles`。

## 7. 開發環境

2026-11-15 草案時,雲端 session 的權限模式擋下了執行容器;2026-11-21 使用者換了權限模式後可以執行,隔離參數實測有效。
dockerd 不會自己常駐:每個雲端 session 要先在背景啟動(`dockerd`)。

## 8. 不做

- 通用 shell、網路存取、多檔專案(`cargo` + 外部 crate)、`runs`(執行比對輸出)。
