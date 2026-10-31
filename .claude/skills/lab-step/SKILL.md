---
name: lab-step
description: AI Engineering Lab(這個 Rust repo)每一個實作步驟的固定流程:TDD、在空殼上確認失敗、實作、mutation 檢查、scripts/check.sh、commit + push、等 CI、寫導讀。使用者說「開始第 N 步」「繼續下一步」、要實作 design 文件實作順序表裡的任何一步、或要 commit 這個 repo 的程式變更時,一定要先用這個 skill——即使只是小改動,也照這個流程走。
---

# lab-step:一步的完整流程

這個 repo 是學習專案(CLAUDE.md):Claude 寫、使用者讀,`git log` 本身就是教材。
所以每一步都要**可信**(測試真的有在測)、**可讀**(一個 commit 一個概念)、**可追**(文件同步更新)。
下面每一條規則都來自實際踩過的坑;理由寫在旁邊,遇到規則沒涵蓋的情況,依理由判斷。

## 0. 開始前

- 讀 `docs/ROADMAP.md` 的「目前位置」與對應的 design 文件(例如 `design-part2.md` 的實作順序表)。
- 這一步如果碰到**設計分岔**(文件沒決定的事),先停下來列 2~3 個方案與取捨給使用者選,選完再寫。
  實作細節(命名、內部結構)自己決定,在導讀裡說明即可。
- 會讓某個 commit 處於不安全狀態的順序要避開。例:gateway 在加上 token 檢查之前,`main` 不開 port。

## 1. TDD:先寫測試,在空殼上確認「因為對的理由」失敗

1. 先寫測試,再寫一個**能編譯但行為錯誤**的空殼(回傳固定值、什麼都不做)。
2. 跑測試,逐一確認失敗原因就是「功能還沒做」,而不是測試自己寫錯。
   測試本身寫錯(例如預期的 JSON 少算了被省略的欄位)就改測試,並在導讀裡說明改的是測試不是產品。
3. **在空殼上就通過的測試要記下來**:通常是在測「不該發生的事」(例如不該重試)。
   它們一定要在第 3 節用 mutation 確認有效,否則可能永遠不會失敗。

寫測試的檢查清單(都是真的發生過「測試用錯的理由通過」):

- **指名是誰擋下的**:拒絕類的測試要確認「是我們要測的元件擋下的」。
  同樣是 `InvalidRequest`,可能是 `validate()` 搶先擋了,根本輪不到 Alias。
  用合法的輸入、比對具體的錯誤訊息、確認上游 provider 的呼叫次數是 0。
- **比對要夠具體**:只比「訊息裡有 `allow_remote`」,另一條檢查的訊息也有這個字。比對整句或整段 JSON。
- **劇本要讓「忽略規則」的實作產生不同結果**:違規之後如果沒有 Finish,忽略違規的實作也會失敗,測不出差別。
- **單一組合測不出順序**:「Host 錯 + token 對」兩種順序都回 403;要加「Host 錯 + 沒 token → 403 而不是 401」。
- **只檢查上限,測不出隨機**:「等待不超過上限」對「永遠等上限」也成立;另外確認數值真的會變。
- **只檢查「沒有」,測不出「有」**:遮罩用的 `Debug` 只斷言「不含內容」,換成什麼都不印也會通過;
  同時斷言印出的結構(`TextDelta { index: 0, bytes: 6 }`)。`<= 4096` 也一樣,空字串也通過,要比對剛好的值。
- **每種合法的寫法都要出現在測試裡**:只測了不合法的 regex,「把所有 regex 都當成不合法」的實作也會通過。
- **可能卡住的測試要加保險**:錯的實作可能讓測試永遠不結束(CI 會卡到逾時)。
  gateway 有 `testing::within()`;其他地方用 `tokio::time::timeout` 包起來,讓它「失敗」而不是「卡住」。
- **時間相關的測試**用 `#[tokio::test(start_paused = true)]`,不真的等。
- **環境變數**:Rust 2024 的 `set_var` 是 unsafe,這個 workspace 禁止 unsafe。把讀環境的部分拆到外層,測試只測純函式。
- `Path` 的 `==` 比的是組成部分(`a/./b == a/b`);要測字串形式就比 `.to_str()`。

## 2. 實作

- 程式註解一律中文;寫 Rust 特有的東西,**優先對照 Java**(CLAUDE.md 學習模式)。業務邏輯註解保持精簡。
- 不用 `unwrap` / `expect` / `panic` / 索引(clippy 會擋);外部輸入一律當不可信。
- 依賴:先看 `cargo tree -i <crate>`。已經在依賴樹裡的 crate,直接宣告比透過別人轉手使用更好。

## 3. Mutation 檢查

故意把程式改壞,確認有測試失敗,而且是**對的那個**測試失敗。用 `scripts/mutate.py`:

```bash
cat > target/mutations.json <<'EOF'
[{"label": "不檢查 token", "file": "crates/gateway/src/guard.rs",
  "find": "    if !valid {", "replace": "    if false {", "expect": "a_missing_token_is_401"}]
EOF
python3 scripts/mutate.py target/mutations.json -- cargo test -p gateway --lib
```

- 每個規則挑一個 mutation:拿掉檢查、反轉條件、換順序、刪掉一行、改常數。
  特別針對第 1 節「在空殼上就通過」的那些測試。
- 結果怎麼看:
  - `CAUGHT`:好。
  - `MISSED`:先判斷是**測試缺口**(補測試,再跑一次確認抓到)還是**等價 mutant**(行為其實沒變,
    例如 axum 的 `.route()` 加在 `.layer()` 之後本來就不受保護)。等價的要在導讀裡說明理由。
  - `WRONG-TEST`:有測試失敗但不是預期的那個 → 檢查 mutation 是不是改出了別的問題(例如重複註冊路由導致 panic)。
  - `NO-BUILD` / `SKIPPED`:mutation 本身寫錯,不算數,重寫。
- `expect` 比對的是**測試函式名稱**(`complete_streams_and_collects_a_response`),不是檔名(`anthropic_http`、`e2e`);
  寫成檔名會變成 WRONG-TEST(真的發生過兩次)。
- 腳本一定會還原檔案;跑完用 `git status` 再確認一次。

手寫的之後,再用 `cargo-mutants` 對這一步改到的 crate 跑一次(gateway 約 1 分鐘):

```bash
cargo mutants -p <crate> --output target --timeout 120 --jobs 3   # 結果在 target/mutants.out/
cargo mutants -p <crate> --file <path> --output target/m2 ...      # 只重跑一個檔案
```

兩者互補:手寫的擅長**語意**(layer 順序、路由放在哪、permit 跟不跟著串流);
cargo-mutants 擅長**機械式**的盲點(`>` 換 `>=` 的邊界、被忽略的欄位、格式化、常數)。
把 `missed.txt` 裡的每一個分成三類,寫進導讀:

- **A 測試缺口**:補測試,重跑確認抓到。
- **B 單元測試碰不到**(例如 `main.rs`):記下來,由整合 / E2E 測試負責。
- **C 等價或不值得測**:說明理由(例如函式結束時本來就會 drop、純輸出的常數)。

`unviable` 是替換後編譯不過,不算數。

C 類確定之後,加進 `.cargo/mutants.toml` 的 `exclude_re`,**每一條都寫理由**(不寫理由就是在藏測試缺口)。
每週排程(`.github/workflows/mutants.yml`)會對整個 workspace 再跑一次,摘要在該次 run 的 Summary 頁。

## 4. 全套檢查

```bash
scripts/check.sh      # fmt、clippy -D warnings、test、cargo deny、私有資料掃描(全部檔案)
```

全部通過才往下。macOS 沒有 `timeout` 指令,不要在腳本裡用它。

## 5. 文件同步

- `docs/ROADMAP.md`「目前位置」:完成到第幾步、下一步是什麼。
- design 文件:實作時發現的新規則或修正補進對應章節(例如「違反協定時記成 error」)。
- 新的跨模組決策加進 ROADMAP 決策表(D-xxx);使用者交代要記住的事寫進待辦。

## 6. Commit、push、等 CI(D-015)

```bash
git add <這一步的檔案>     # 不要 git add -A 整個 repo:避免把暫存檔帶進去
git commit -m "feat(<crate>): step N - <一句話>" -m "Co-Authored-By: ..."
git push
gh run watch "$(gh run list --limit 1 --json databaseId -q '.[0].databaseId')" --exit-status
```

- pre-commit / commit-msg hook 會自動做私有資料掃描(`scripts/private-scan.sh`);被擋下就移除內容,
  不要用 `--no-verify` 繞過。新 clone 要先跑一次 `scripts/install-hooks.sh`。
- 私有資料的規則在 `CLAUDE.local.md`;關鍵字在 gitignore 的 `.private-patterns`。兩者的內容都不准出現在 repo。
- 不讀 `.env`(有 hook 擋);要用 key 時讓程式自己讀,或在命令列用假的值覆寫。
- CI 失敗就在下一個 commit 修,並在導讀裡說明。

## 7. 導讀(回覆使用者)

用這個結構,繁體中文:

1. **一句話結果**:commit hash、CI 結果、測試數、哪些檢查通過(誠實寫出沒做的,例如「fsync 無法用單元測試驗證」)。
2. **這一步做了什麼**:表格,檔案 → 內容。
3. **導讀**:依閱讀順序列出 `檔案:行號`,每段說「看哪幾行、為什麼這樣寫、Java 會怎麼寫」。
   新的 Rust 概念標出來;使用者卡住時改用「小步驟 + 每步確認」的講法。
4. **Mutation 檢查**:結果表;漏掉的、用錯理由通過的、等價 mutant 都要說清楚學到什麼。
5. **選做小練習**(5~15 分鐘):讓使用者改一點東西、跟著 compiler 錯誤或測試失敗走。
6. **下一步**:是什麼、有沒有需要使用者決定的事,然後問要不要繼續。
