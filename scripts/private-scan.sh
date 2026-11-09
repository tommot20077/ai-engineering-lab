#!/usr/bin/env bash
# 私有資料與秘密掃描(CLAUDE.md「私有資料」)。
#
#   scripts/private-scan.sh --staged          掃描暫存區的檔案內容與檔名(pre-commit hook 用)
#   scripts/private-scan.sh --message FILE    掃描 commit 訊息(commit-msg hook 用)
#   scripts/private-scan.sh --all             掃描所有追蹤中的檔案(偶爾做一次全面檢查)
#
# 關鍵字分兩種:
#   - 秘密的格式(API key):直接寫在下面,公開也無妨。
#   - 私有專案的名稱:放在 gitignore 的 .private-patterns,一行一個正規表示式,只存在本機。
#     雲端 session 是重新 clone 的,沒有這個檔案:改從環境變數 PRIVATE_PATTERNS 讀
#     (用 `|` 或換行分隔,設在雲端環境的設定裡)。兩者都沒有時,雲端直接拒絕(fail-closed)。
# 比對不分大小寫。命中時只印出「檔案:行號」,不印內容:萬一命中的是 key,不要再印到終端機上。

set -euo pipefail

root=$(git rev-parse --show-toplevel)
# 前綴拆開寫:否則這個檔案自己就含有「key 的格式」,會被自己擋下來(真的發生過)。
key_prefix='sk-ant'
secret_re="${key_prefix}-api|${key_prefix}-[A-Za-z0-9_-]{30,}"
patterns_file="$root/.private-patterns"

re="$secret_re"
private_re=""
if [[ -f "$patterns_file" ]]; then
  private_re=$(grep -vE '^[[:space:]]*(#|$)' "$patterns_file" | paste -sd '|' - || true)
elif [[ -n "${PRIVATE_PATTERNS:-}" ]]; then
  # 換行也當分隔;再把多餘的 `|` 收掉。空的選項(`a||b`、結尾的 `|`)會比對到每一行,要先清掉。
  private_re=$(printf '%s' "$PRIVATE_PATTERNS" | tr '\n' '|' | sed -E 's/\|+/|/g; s/^\|//; s/\|$//')
fi

if [[ -n "$private_re" ]]; then
  re="$re|$private_re"
elif [[ "${CLAUDE_CODE_REMOTE:-}" == "true" ]]; then
  # 雲端在沒有關鍵字的情況下 commit,私有名稱就會直接進公開 repo:寧可擋下(寫入路徑 fail-loud)。
  echo "private-scan: no .private-patterns and no PRIVATE_PATTERNS in this cloud session; refusing." >&2
  echo "Set PRIVATE_PATTERNS in the cloud environment settings (see README)." >&2
  exit 1
else
  echo "private-scan: $patterns_file not found; scanning for secrets only" >&2
fi

# 規則本身寫錯(例如少了右括號)時,grep 會回 2;下面的 `|| true` 會把它吞掉,
# 結果是「什麼都沒擋」卻顯示通過。所以先試一次,壞掉就停。不印出規則內容:它本身就是私有的。
rc=0
grep -iqE "$re" </dev/null 2>/dev/null || rc=$?
if [[ $rc -eq 2 ]]; then
  echo "private-scan: the patterns are not a valid regular expression; refusing." >&2
  exit 1
fi

hits=()

# 掃描一段文字(從 stdin 讀);命中就記下「名稱:行號」。
# 注意:一定要用重新導向(`scan_text x < file`、`scan_text x < <(cmd)`)餵資料,不能用管線
# (`cmd | scan_text x`):管線右邊在「子 shell」裡執行,加進 hits 的東西出了子 shell 就消失,
# 結果是什麼都沒擋下來。這個 bug 真的發生過一次。
scan_text() {
  local name=$1
  local lines
  lines=$(grep -inE "$re" | cut -d: -f1 || true)
  for line in $lines; do
    hits+=("$name:$line")
  done
}

case "${1:---staged}" in
  --staged)
    # 暫存區裡「將要 commit 的版本」(`git show :path`),不是工作目錄裡的檔案。
    while IFS= read -r -d '' path; do
      if printf '%s\n' "$path" | grep -iqE "$re"; then
        hits+=("$path (file name)")
      fi
      scan_text "$path" < <(git show ":$path")
    done < <(git diff --cached --name-only -z --diff-filter=ACMR)
    ;;
  --message)
    file=${2:?usage: private-scan.sh --message FILE}
    # 拿掉 git 自動加的註解行(# 開頭)再掃描。
    scan_text "commit message" < <(grep -v '^#' "$file" || true)
    ;;
  --all)
    while IFS= read -r -d '' path; do
      scan_text "$path" < "$path"
    done < <(git ls-files -z)
    ;;
  *)
    echo "usage: private-scan.sh [--staged | --message FILE | --all]" >&2
    exit 2
    ;;
esac

if ((${#hits[@]} > 0)); then
  echo "private-scan: found private names or secrets; nothing was committed:" >&2
  printf '  %s\n' "${hits[@]}" >&2
  echo "Remove them (see CLAUDE.local.md). Only file:line is shown on purpose." >&2
  exit 1
fi
