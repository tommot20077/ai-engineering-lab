# 003 Agent Runtime 第三部分 — 沙箱(草案)

> 狀態:**草案(2026-11-15),等使用者確認**。使用者交代「003 做完再一起看」,所以每個分岔由 Claude 先選建議方案,
> 標成 **【Claude 代選,待確認】**;使用者看過、可以推翻,定案後才編 D-xxx 寫進 ROADMAP 決策表。
> **實作還沒開始**:雲端開發環境不允許執行容器,見 §7,這件事要先決定。
> 第一部分見 [`design.md`](design.md),第二部分見 [`design-part2.md`](design-part2.md)。

## 1. 要解決什麼

讓 agent 做有副作用的事,而且是對**不可信的輸入**做:

- **寫檔**:個人助理(005)要能寫筆記、草稿。路徑是模型給的。
- **跑程式**:驗證一段程式碼能不能編譯、執行結果是什麼。程式碼是模型寫的。
- **eval 的 `compiles` 檢查**(D-040):回答裡的 Rust 程式碼到底能不能編譯。002 第二部分把它延到這裡,因為要先有隔離環境。

**威脅模型**:模型可能被 prompt injection 操縱(工具結果、MCP 的說明、檔案內容都是入口),或單純寫出惡意、失控的程式碼。要防的是:

| 威脅 | 例子 |
|---|---|
| 讀寫根目錄以外的檔案 | `../../.ssh/id_ed25519`、指向外面的 symlink、寫入前一刻被換成 symlink(TOCTOU) |
| 洩漏資料 | 程式碼把讀到的東西送到外部網路 |
| 讀秘密 | 環境變數裡的 API key |
| 耗盡資源 | fork bomb、無窮迴圈、塞爆磁碟或記憶體、輸出幾 GB |
| 留下東西 | 背景行程、暫存檔 |

**編譯也是執行**:`include_str!("/Users/me/.ssh/id_ed25519")` 在**編譯時**讀檔,內容會出現在錯誤訊息或執行檔裡。
單檔 `rustc` 沒有 build script 和 proc macro 依賴,但光是 `include_str!` 就足以說明:編譯不可信的程式碼也需要隔離。

## 2. 決策(待確認)

### P3-1 範圍 【Claude 代選,待確認:A】

| 方案 | 內容 | 取捨 |
|---|---|---|
| **A** | 寫檔工具 + 在沙箱裡編譯 / 執行單檔 Rust 的工具 + eval 的 `compiles` 檢查 | 涵蓋三個目標;「跑程式」限定在單檔 Rust,攻擊面可控 |
| B | 只做寫檔 | 最小;但 D-040 的 `compiles` 還是沒著落 |
| C | 通用的 shell 工具(`run_command`) | 最有用也最危險;005 目前不需要 |

### P3-2 隔離方式 【Claude 代選,待確認:A】

| 方案 | 內容 | 取捨 |
|---|---|---|
| **A** | **容器**(Docker 或 Podman):`--network none`、唯讀根目錄、只掛一個暫存的工作目錄、非 root、`--cap-drop ALL`、`no-new-privileges`、記憶體 / CPU / 行程數 / 時間上限、跑完 `--rm` | 使用者的 Mac(Docker Desktop、Podman)與 Linux CI 行為一樣;隔離是成熟的機制。代價:要裝容器執行環境,每次啟動幾百毫秒 |
| B | 自己用 Linux namespaces + seccomp(`unshare`、`clone`) | 學得最多;但只能在 Linux 上跑(使用者的主機是 Mac),而且要寫對非常難 |
| C | 只用行程層級的限制(暫存目錄、清空環境變數、rlimit、逾時) | 簡單;但**不是隔離**:上面的 `include_str!` 一樣讀得到任何檔案 |

用 trait 劃邊界(`Sandbox`):Docker 與 Podman 是已知會變的地方(CLAUDE.md 原則),單元測試用假的。

### P3-3 寫檔的路徑安全 【Claude 代選,待確認:A】

| 方案 | 內容 | 取捨 |
|---|---|---|
| **A** | `cap-std`:以目錄的 handle 為根,每一段都用 `openat` 開,symlink 帶不出去 | 也順便解決第一部分記下的 TOCTOU(design.md §7);`read_file`、`list_dir` 一起改用它,三個工具同一套規則 |
| B | 沿用 `canonicalize` + 寫入前再檢查一次 | 改動小;但檢查與寫入之間還是有空檔,而寫入不能復原 |

### P3-4 有副作用的工具怎麼開 【Claude 代選,待確認:A】

| 方案 | 內容 | 取捨 |
|---|---|---|
| **A** | 每次執行明確開啟:`--allow-write`、`--allow-run` 旗標,預設關;沒開的工具不註冊(模型連「有這種工具」都不知道,同 `--root`) | loop 不必改;要用才開 |
| B | 每次呼叫互動確認(y/n) | 最安全;但要 D-042 說的「人工核准」狀態機,loop 要能暫停、接續 |
| C | 設定檔寫了就一直開 | 最方便;忘記關的時候最危險 |

### P3-5 寫檔的語意 【Claude 代選,待確認:A】

| 方案 | 內容 | 取捨 |
|---|---|---|
| **A** | 新建或覆寫根目錄內的 UTF-8 文字檔;先寫暫存檔再 `rename`(不會寫一半);大小上限;拒絕 `.` 開頭的路徑(同讀檔) | 簡單、原子 |
| B | 只能新建,不能覆寫 | 最保守;改筆記就做不到 |
| C | 支援部分修改(patch / 取代一段) | 省 token;規則多、容易寫錯,之後再加 |

### P3-6 `compiles` 檢查 【Claude 代選,待確認:A】

| 方案 | 內容 | 取捨 |
|---|---|---|
| **A** | 新的確定性檢查 `compiles`:取回答裡**最後一個** ` ```rust ` 區塊,在沙箱裡 `rustc --edition 2024`(單檔、不能用外部 crate),結束碼 0 才通過;編譯錯誤存進結果 | D-040 要的就是這個;資料集的 `compiles` 題要改成「寫出完整、能獨立編譯的程式」 |
| B | 編譯並執行、比對輸出(`runs`) | 更強;用同一個沙箱,之後再加 |

### P3-7 沙箱的 image 【Claude 代選,待確認:A】

| 方案 | 內容 | 取捨 |
|---|---|---|
| **A** | 官方的 `rust:<版本>-slim`,用 digest 鎖定,版本跟 `rust-toolchain.toml` 一致 | 不必維護 Dockerfile;digest 鎖住,image 被換掉會失敗而不是默默變了 |
| B | 自己 build image | 可以更小;多一個要維護的東西 |

**小決定(Claude 代選,待確認)**:沙箱的預設上限 —— 編譯 20 秒、執行 5 秒、記憶體 512 MiB、行程數 64、輸出 64 KiB、
工作目錄(tmpfs)64 MiB;程式碼 64 KiB。新的 crate `crates/sandbox`(agent 的工具與 eval 的檢查都用它;eval 已經依賴 agent,
但沙箱不需要知道 agent)。

## 3. 結構(照上面的建議)

```
crates/sandbox/            沙箱(不知道 agent、eval 存在)
  src/lib.rs               Sandbox trait、Limits、Output(結束碼、stdout / stderr 的開頭、是否逾時)
  src/docker.rs            DockerSandbox:組 `docker run` 的參數(單元測試鎖住每一個參數)、執行、收結果
  src/fake.rs              測試用
crates/agent/src/tools/
  files.rs                 Root 改用 cap-std;新的 write_file
  rust.rs                  run_rust:在沙箱裡編譯並執行單檔 Rust
crates/eval/src/           Check::Compiles
```

## 4. 實作順序(待確認後才開始)

| # | 內容 | 主要 Rust 概念 |
|---|---|---|
| 1 | `crates/sandbox`:trait、上限、`DockerSandbox`(參數組裝的單元測試 + 真的容器的整合測試) | `std::process::Command` 的參數、逾時與收掉容器 |
| 2 | 寫檔:`Root` 改用 cap-std、`write_file`、`--allow-write` | capability-based API(`Dir` handle 取代路徑字串) |
| 3 | `run_rust` 工具、`--allow-run` | 把不可信的程式碼交給 trait object |
| 4 | eval 的 `compiles` 檢查;資料集的 `compiles` 題改寫 | 在既有的 `Check` enum 加 variant(跟著 compiler 走) |
| 5 | E2E、README、retro | — |

## 5. 測試

- 單元:`docker run` 的參數(一個一個鎖住:`--network none`、`--read-only`、`--cap-drop ALL`……)、程式碼區塊的擷取、路徑規則。
- 整合(要真的容器):逃逸測試清單 —— 連網路、讀 `/etc`、寫根目錄、fork bomb、無窮迴圈、大量輸出、`include_str!` 讀主機檔案、
  留下背景行程,每一項都要被擋下或被上限停住。設 `SANDBOX_DOCKER=1` 時才跑;設了卻找不到 docker 就**失敗**(不是略過),CI 設它。
- E2E:agent 用 `run_rust` 回答一個要算的問題;eval 對幾個已知能 / 不能編譯的回答跑 `compiles`。

## 6. 不做

- 通用 shell、網路存取、多檔專案(`cargo` + 外部 crate)、互動核准、部分修改檔案。

## 7. 實作前要決定:開發環境不允許執行容器

2026-11-15 在雲端 session 試過:`dockerd` 可以啟動、image 可以拉,但**執行容器(`docker run`)被權限系統擋下**。
沙箱是這部分的安全邊界,必須在開發時親眼看過逃逸測試一項一項被擋下,所以選項是:

| 選項 | 內容 |
|---|---|
| a | 使用者在權限設定裡允許 `docker run`(或整個 docker 指令)→ 照常在雲端做 |
| b | 第三部分改在使用者自己的 Mac(Docker Desktop 或 Podman)開 session 做 |
| c | 先做不需要容器的部分(第 2 步寫檔、第 4 步的資料集與介面),容器的部分只在 CI 驗證 —— **不建議**:安全邊界沒有在開發時跑過就推上去,違反「根基要打穩」 |

另外,如果 P3-2 改選 B(自己用 namespaces),同樣需要能在這裡執行它們的權限,而且使用者的 Mac 上不能用。
