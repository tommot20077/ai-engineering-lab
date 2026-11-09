#!/usr/bin/env bash
# Claude Code 的 PreToolUse hook(設定在 .claude/settings.json):擋下會把環境變數印出來的 Bash 指令。
#
# 為什麼需要:雲端 session 沒有 .env,API key 只能放在環境變數(雲端環境的設定)。
# 一旦執行 `env`、`printenv`、`echo $ANTHROPIC_API_KEY`,key 就會進到對話紀錄裡。
# 這是一道護欄,不是安全邊界:刻意繞過(例如寫個程式去讀)一定擋不住;
# 它擋的是「順手打了一個會印出 key 的指令」這種意外。
#
# 協定:Claude Code 從 stdin 傳入 JSON(`tool_input.command` 是要執行的指令);
# exit 2 = 擋下,stderr 的內容會回給 Claude 說明原因;exit 0 = 放行。

set -euo pipefail

input=$(cat)
# 用 python3 解析 JSON(macOS 與雲端的 Ubuntu 都有),拿不到就當成空字串,不要因為格式問題擋掉所有指令。
cmd=$(printf '%s' "$input" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("tool_input",{}).get("command",""))
except Exception: print("")')

# 每一條都是「印出環境變數」的常見寫法。`env VAR=值 指令`(帶著設定執行)是正常用法,不擋;
# 在命令列用假的值覆寫(`ANTHROPIC_API_KEY=fake cargo test`)也不擋,因為那是寫入,不是讀出。
rules=(
  '(^|[;&|(`]|\$\()[[:space:]]*env[[:space:]]*($|[;&|)`>])'   # 單獨的 env(沒有接 VAR=值)
  '(^|[^[:alnum:]_])printenv([^[:alnum:]_]|$)'                 # printenv、printenv VAR
  '\$\{?[A-Za-z_]*(KEY|TOKEN|SECRET|PASSWORD|PATTERNS)'         # $ANTHROPIC_API_KEY、${GATEWAY_TOKEN}、$PRIVATE_PATTERNS
  '/proc/[^[:space:]]*/environ'                                # /proc/self/environ
  '(^|[;&|(][[:space:]]*)(export|declare|typeset)[[:space:]]+-[a-z]*[px]' # export -p、declare -x
  '(^|[;&|(][[:space:]]*)set[[:space:]]*($|[;&|)>])'           # 單獨的 set 會印出所有變數
)

for rule in "${rules[@]}"; do
  if printf '%s\n' "$cmd" | grep -qE "$rule"; then
    echo "guard-env: blocked a command that would print environment variables (secrets live there in cloud sessions)." >&2
    # 建議的寫法本身不能被擋:`[[ -v 名稱 ]]` 只檢查有沒有設定,不展開值,也沒有 `$`。
    echo "To check whether a variable is set without printing it: [[ -v ANTHROPIC_API_KEY ]] && echo set" >&2
    exit 2
  fi
done
exit 0
