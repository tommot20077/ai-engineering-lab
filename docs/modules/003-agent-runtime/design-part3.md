# 003 Agent Runtime 第三部分 — 沙箱與有副作用的工具

> 狀態:**完成(2026-11-21,第 1~7 步)**,回顧見 [`retro.md`](retro.md) 的第三部分。設計:**已定案(2026-11-21)**。草案(2026-11-15)由 Claude 先選建議方案,使用者逐一確認;P3-4 改成「旗標 + 每次核准」兩個都要,
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

## 7.1 實作筆記(第 1 步,2026-11-21)

做的時候才發現、或實測之後才決定的事:

- **每次一個容器,分四步**:`run --detach … sleep N` → `exec --interactive … cat`(原始碼走 stdin)→ `exec rustc` → `exec /work/main` → `rm --force`。
  編譯與執行分開計時、分開收輸出;逾時由**主機**判斷(容器裡的 `timeout` 跟不可信的程式同一個使用者,可以被 kill)。
  `sleep N` 是所有步驟上限的總和加寬限:我們被 SIGKILL、來不及收掉時,容器時間到自己結束,`--rm` 刪掉它。
- **docker 的 tmpfs 預設是 `noexec`、擁有者是 root**:nobody 寫不進去、編譯出來的程式不能執行。
  改成 `--tmpfs /work:rw,exec,nosuid,nodev,size=…,uid=65534,gid=65534,mode=0700`。
- **`TMPDIR=/work`**:rustc 不需要(暫存檔放在 `-o` 的目錄),但程式用 `std::env::temp_dir()` 時要落在可寫的地方;逃逸測試有測。
- **CapBnd**:不是 root 時 CapEff、CapPrm 本來就是 0,看不出 `--cap-drop ALL` 有沒有效;要看上限 CapBnd(mutation 檢查時發現)。
- **`--pull never`**:沙箱不自動下載 image(1 GB 以上,不該在工具呼叫裡默默發生);不在本機時錯誤訊息附上 `docker pull` 指令。
  image 必須是 `name@sha256:<64 hex>`,不然建立沙箱時就失敗。
- **取消也要收掉容器**:`Container` 守衛(RAII);future 被 drop 時在背景送 `docker rm --force`。
- **Docker Hub 對雲端環境回 429**(匿名下載次數上限):改從 Google 的 Docker Hub 鏡像 `mirror.gcr.io/library/rust` 拉,digest 一樣,
  再 `docker tag` 成 `rust:1.99.1-slim`。CI(GitHub Actions)直接從 Docker Hub 拉。
- **沒有 swap 的機器上**,`--memory-swap` 拿掉看不出差別(這台雲端 VM 就是);參數由單元測試鎖住。

## 7.2 實作筆記(第 2 步,2026-11-21)

- **API**:`Run::next()` 回傳 `RunEvent::Step` 或 `RunEvent::NeedsApproval(Vec<Approval>)`;`run.resolve(vec![(call_id, Decision)])`
  每個等核准的呼叫都要剛好回答一次,對不上(少、多、重複、沒有在等)就回 `ResolveError`、狀態不變。沒回答就 `next()`,拿到同一份清單。
- **`ApprovalPolicy::{Ask, AutoApprove}`** 放在 `AgentConfig`:預設 `Ask`;eval 用 `AutoApprove`(D-056 的「eval 一律不問」);CLI 的 `--yes` 在第 3 步。
- **`Agent::run()`(一次跑完)遇到要核准的呼叫:全部拒絕**(沒有人可以問,fail-closed),模型收到「使用者拒絕」。
- **同一輪裡沒有副作用的工具,也等核准之後才跟著執行**(§3 寫「照常執行」,意思是它們不需要核准):暫停的狀態只有「整輪都還沒執行」一種,
  第 4 步存檔時不必存半輪的結果;讀檔、時間之類的結果也是核准當下的,不是幾小時前的。
- **檢查順序**:重複的 id → 工具呼叫數上限 → 核准。整輪會被上限擋下時不拿來問人;參數超過大小上限的呼叫不問(它們不會被解析、也不會執行)。
- **時間**:`used`(已經用掉的)+ `since`(這一段開始的時間);暫停時把這一段加進 `used`,`resolve()` 時重設 `since`。
- **停在等核准時 `finish()`**:結果是 `Stopped`,那一輪(模型的回應,用量已經算進去)也放進 `steps`,工具結果是空的。
- 拒絕的呼叫:`is_error: true`,結果是固定字串 `DENIED`,`duration_ms: 0`。

## 7.3 實作筆記(第 3 步,2026-11-21)

- **`Root` 搬到 `tools/root.rs`**,四個檔案工具共用:字串規則(絕對路徑、`..`、隱藏的段、1024 bytes)→ 從根目錄 handle 一段一段
  `open_dir_nofollow` → 最後一段:讀檔先看 `symlink_metadata`(不是一般檔案就不開,FIFO 開起來會卡住)、再用 `FollowSymlinks::No` 開。
- **行為改變:指向根目錄裡面的 symlink 也不再跟隨**(第一部分允許)。規則越簡單越不容易錯;寫入時不會被 symlink 帶到別的地方。
  隱藏檔的錯誤訊息從「are not readable」改成「are not allowed」(寫入也用同一條規則)。
- **寫入**:`.<名字>.<uuid>.tmp`(隱藏檔,工具看不到)`create_new` + nofollow → 寫入 → 覆寫時套用原本的權限 → `sync_all` → `rename` 蓋過去;
  失敗就刪掉暫存檔。上層目錄必須存在(不幫模型建目錄)。
- **`edit_file` 的「剛好一次」算重疊**:`"aa"` 在 `"aaa"` 裡算兩次。核准之後執行時重新讀檔、重新找(核准時看到的可能已經變了)。
- **核准提示的說明來自模型**:印到終端機之前把控制字元與方向控制、零寬字元換成 `?`(`cli::terminal_safe`);只有 `y` / `yes` 是核准,
  stdin 結束也是拒絕。讀 stdin 放在 `spawn_blocking`。
- **軌跡**:新事件 `approval`(誰同意了什麼),`start` 加 `auto_approve`(舊檔讀成 false)。
- 測不到、只寫在註解的防線(mutation 檢查時確認是等價):最後一段的 `FollowSymlinks::No`(前面的 `symlink_metadata` 已經擋了,
  只差在檢查與開檔之間被換掉的競爭)、暫存檔的 `create_new`、`sync_all`、失敗時刪暫存檔。

## 7.4 實作筆記(第 4 步,2026-11-21)

- **怎麼存檔**:在核准提示回答 `s`。結束碼 3(0 回答了、1 沒回答、2 沒開始之外的新狀態)。
- **兩層**:library 的 `Suspended`(agent loop 的狀態:對話、計數、已用時間、暫停的那一輪,`version: 1`、`deny_unknown_fields`);
  CLI 的 `SavedRun` = `RunPlan`(重建 agent 用的選項:模型別名、system prompt 的**內容**、`--root` 的絕對路徑、`--allow-write`、record)+ 軌跡路徑 + `Suspended`。
- **`Agent::resume` 比對**模型、system prompt、`max_tokens`、護欄、工具定義;任何一個不同就拒絕。
- **等核准的清單接續時重新算**,不信存檔裡的(存檔裡的只是紀錄)。`AutoApprove` 時清單是空的,接著直接執行。
- **順序**:讀存檔 → 重建 agent、`resume` 檢查 → 打開軌跡 → **認領**(刪掉存檔)→ 才開始執行。檢查失敗時存檔還在;
  兩個行程同時接續,只有一個刪得掉存檔。
- **時間**:`used_ms` 存進檔案,接續時接著算(等待的時間不算)。
- **CI 上的逾時**(第 2 步的 Mutants (diff) 有 5 個、這一步手寫的 mutation 有 2 個):讓 `resolve` 什麼都不做的 mutant 會讓
  「拉到結束」的迴圈永遠停在等核准;`next()` 不會讓出執行緒,tokio 的逾時插不進來,只能等 cargo-mutants 的 300 秒。
  修法:測試用數步數的 `drain()`;`Agent::run` 問完還有人在等就停(產品端也不會空轉)。

## 7.5 實作筆記(第 5 步,2026-11-21)

- **`[sandbox]` 設定的形狀放在 sandbox crate(`SandboxSection`)**:agent.toml 與 eval.toml 共用;單位是秒、MiB、KiB,
  `to_config()` 用 `ContainerSandbox::new` 的同一套規則檢查。載入設定時就檢查(用不用 `run_rust` 都一樣)。
- **`limits.tool_timeout_secs` 要比沙箱的編譯 + 執行長**:只在 `--allow-run` 時檢查(不用 `run_rust` 的設定不受影響)。
- **`run_rust` 只拿到 `Arc<dyn Sandbox>`**:測試用假的沙箱(記下收到的程式、回傳事先給的結果);E2E 用假的 docker(shell script);
  `SANDBOX_DOCKER=1` 時再對真的容器跑一次。
- **給模型的結果**:`compile: ok` / `compile: failed (…)` + rustc 的 stderr;`run: exit code N`、逾時、137 註明「可能是記憶體用完」;
  stdout、stderr 原樣附上,截斷時註明。沙箱本身不能用(沒有 docker、image 不在)是工具錯誤,訊息原樣交回模型。
- **說明裡的上限是設定的值**(`RunRust::new` 時組好):模型知道編譯、執行各有幾秒。
- `RunPlan` 加 `allow_run`:存檔接續時一樣有 `run_rust`。

## 7.6 實作筆記(第 6 步,2026-11-21)

- **`Check::Compiles {}`**(JSON `{"type": "compiles"}`)。寫成沒有欄位的 struct variant:serde 對 internally tagged 的 unit variant
  不套用 `deny_unknown_fields`,`{"type": "compiles", "edition": "2021"}` 會被默默接受(測試抓到的)。
- **取最後一個 ` ```rust ` 區塊**:回答常先引用題目、最後才給修好的版本。語言標記第一個字是 `rust`(`rust,ignore` 也算、`rustc` 不算);
  其他語言的區塊配對到收尾為止,裡面的字樣不算;沒收尾的不算。
- **結果**:編譯成功 → 通過;沒有區塊、編譯失敗、逾時 → 沒通過(理由只放 rustc 第一行 `error…`,不放程式碼:回答是私有內容);
  沒有沙箱、沙箱壞了 → 錯誤(不是模型的錯,修好可以重新打分)。
- **`Checks` 帶一個 `Option<Arc<dyn Sandbox>>`**,經由 `ScoreSetup.checks` 交給打分流程;`eval score` 用 `[sandbox]`(eval.toml,形狀同 agent.toml)
  建立 `ContainerSandbox`。資料集沒有 `compiles` 時不會碰到 docker。
- **資料集**:新的 `evals/rust-fix.jsonl`(8 題),由 `rust-intermediate` 的 8 題編譯錯誤題改寫:「修好它、保持原本想做的事、不准 unsafe、
  給完整程式」;檢查 `compiles` + `not_contains "unsafe"`,評分標準「保持原本的意圖」(擋掉「刪掉出問題的程式」這種修法)。
  另開新檔而不是改 `rust-intermediate`:改了它的 hash 就變了,之前的執行不能再拿來比較。
- **資料集測試用本機 rustc 的 `Sandbox`**(`LocalRustc`):只給自己寫的參考答案用(可信);負對照確認題目裡原本的程式編譯不過。

## 8. 不做

- 通用 shell、網路存取、多檔專案(`cargo` + 外部 crate)、`runs`(執行比對輸出)。
