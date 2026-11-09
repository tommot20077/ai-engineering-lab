#!/usr/bin/env bash
# 測試兩道護欄:scripts/guard-env.sh(擋下印出環境變數的指令)與 scripts/private-scan.sh
# (私有資料掃描,含雲端的 PRIVATE_PATTERNS 與 fail-closed)。scripts/check.sh 與 CI 都會跑。
#
#   scripts/test-guards.sh
#
# 掃描的部分在暫存的 git repo 裡跑,用的是假的關鍵字(zorbax、quuxcorp),不碰真正的 .private-patterns。
set -uo pipefail
REPO=$(git rev-parse --show-toplevel)
fail=0
check() { # 名稱 預期 實際
  if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1 (expected $2, got $3)"; fail=1; fi
}

# ---------- guard-env ----------
guard() { python3 -c 'import json,sys; print(json.dumps({"tool_input":{"command":sys.argv[1]}}))' "$1" \
  | "$REPO/scripts/guard-env.sh" 2>/dev/null; echo $?; }
D='$'
for c in "env" "env | grep KEY" "printenv" "printenv HOME" "echo ${D}ANTHROPIC_API_KEY" "echo \"${D}{GATEWAY_TOKEN}\"" \
         "cat /proc/self/environ" "export -p" "declare -x" "set" "ls; env" "x=\$(env)" "echo ${D}PRIVATE_PATTERNS"; do
  check "block: $c" 2 "$(guard "$c")"
done
for c in "cargo test --workspace" "env RUST_LOG=debug cargo run" "ANTHROPIC_API_KEY=fake cargo test" \
         "cp .env.example .env" "set -euo pipefail" "git commit -m 'environment'" "[[ -v ANTHROPIC_API_KEY ]] && echo set" \
         "export FOO=bar" "grep -rn environ src" "scripts/check.sh"; do
  check "allow: $c" 0 "$(guard "$c")"
done
check "bad json is allowed" 0 "$(echo 'not json' | "$REPO/scripts/guard-env.sh" 2>/dev/null; echo $?)"

# ---------- private-scan ----------
T=$(mktemp -d)
cd "$T" && git init -q && mkdir scripts && cp "$REPO/scripts/private-scan.sh" scripts/
scan() { scripts/private-scan.sh --staged >/dev/null 2>&1; echo $?; }
echo "hello" > clean.txt && git add clean.txt
check "local, no patterns, clean -> pass" 0 "$(env -u PRIVATE_PATTERNS -u CLAUDE_CODE_REMOTE bash -c 'scripts/private-scan.sh --staged >/dev/null 2>&1; echo $?')"
check "cloud, no patterns -> refuse" 1 "$(env -u PRIVATE_PATTERNS CLAUDE_CODE_REMOTE=true bash -c 'scripts/private-scan.sh --staged >/dev/null 2>&1; echo $?')"
check "cloud, env patterns, clean -> pass" 0 "$(PRIVATE_PATTERNS='zorbax|quuxcorp' CLAUDE_CODE_REMOTE=true scan)"
echo "we use QuuxCorp here" > leak.txt && git add leak.txt
check "env patterns catch name (case-insensitive)" 1 "$(PRIVATE_PATTERNS='zorbax|quuxcorp' scan)"
check "newline-separated env patterns" 1 "$(PRIVATE_PATTERNS=$'zorbax\nquuxcorp\n' scan)"
check "trailing and double bars do not match everything" 1 "$(PRIVATE_PATTERNS='|quuxcorp||' scan)"
git rm -q --cached leak.txt
check "trailing bars: clean file still passes" 0 "$(PRIVATE_PATTERNS='zorbax||' scan)"
check "newline list with blank lines: clean file still passes" 0 "$(PRIVATE_PATTERNS=$'zorbax\n\nfoobaz\n' scan)"
check "invalid regex -> refuse" 1 "$(PRIVATE_PATTERNS='zorbax(' scan)"
printf 'quuxcorp\n' > .private-patterns
git add leak.txt
check "file wins and catches name" 1 "$(unset PRIVATE_PATTERNS; scan)"
rm -rf "$T"
exit $fail
