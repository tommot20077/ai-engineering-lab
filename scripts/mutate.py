#!/usr/bin/env python3
"""手寫 mutation 檢查:故意把程式改壞,確認有測試會失敗,而且是「對的那個」測試失敗。

    python3 scripts/mutate.py MUTATIONS.json -- cargo test -p gateway --lib

MUTATIONS.json 是一個陣列,每一項是一個 mutation:

    [{"label": "不檢查 token", "file": "crates/gateway/src/guard.rs",
      "find": "    if !valid {", "replace": "    if false {",
      "expect": "a_missing_token_is_401"}]

- `find` 在檔案裡必須剛好出現一次(避免改到別的地方)。
- `expect`(可選):預期會失敗的測試名稱片段。失敗的測試裡沒有它,就標成 WRONG-TEST:
  有測試失敗,但不是我們以為會抓到它的那個,常常代表「用錯誤的理由通過」或改出了別的問題。
- 每個 mutation 跑完一定還原檔案(包含被 Ctrl+C 中斷時)。

結果:
- CAUGHT       有測試失敗(而且符合 expect)
- WRONG-TEST   有測試失敗,但不是預期的那個 → 檢查 mutation 或測試
- MISSED       測試全部通過 → 測試缺口,或等價 mutant(行為其實沒變)
- NO-BUILD     編譯不過 → mutation 本身寫錯,不算數
- TIMEOUT      超過時間 → 可能卡住了,要人工看
"""

import json
import re
import subprocess
import sys
from pathlib import Path

TIMEOUT_SECONDS = 600
FAILED_RE = re.compile(r"^test (\S+) \.\.\. FAILED$", re.MULTILINE)
RESULT_RE = re.compile(r"^test result: .*$", re.MULTILINE)


def run_one(mutation: dict, command: list[str]) -> tuple[str, str]:
    path = Path(mutation["file"])
    original = path.read_text(encoding="utf-8")
    count = original.count(mutation["find"])
    if count != 1:
        return "SKIPPED", f"`find` appears {count} times (must be exactly 1)"
    try:
        path.write_text(original.replace(mutation["find"], mutation["replace"], 1), encoding="utf-8")
        try:
            proc = subprocess.run(
                command, capture_output=True, text=True, timeout=TIMEOUT_SECONDS
            )
        except subprocess.TimeoutExpired:
            return "TIMEOUT", f"no result within {TIMEOUT_SECONDS} s"
    finally:
        path.write_text(original, encoding="utf-8")

    output = proc.stdout + proc.stderr
    if not RESULT_RE.search(output):
        return "NO-BUILD", "did not compile (the mutation itself is invalid)"
    failed = FAILED_RE.findall(output)
    if not failed:
        return "MISSED", "all tests passed"
    expect = mutation.get("expect")
    detail = ", ".join(failed)
    if expect and not any(expect in name for name in failed):
        return "WRONG-TEST", f"expected `{expect}`, but failed: {detail}"
    return "CAUGHT", detail


def main() -> int:
    if "--" not in sys.argv or sys.argv.index("--") != 2:
        print(__doc__)
        return 2
    mutations = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
    command = sys.argv[3:]
    worst = 0
    for mutation in mutations:
        status, detail = run_one(mutation, command)
        print(f"[{status:<10}] {mutation['label']}: {detail}", flush=True)
        if status != "CAUGHT":
            worst = 1
    return worst


if __name__ == "__main__":
    sys.exit(main())
