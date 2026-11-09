#!/usr/bin/env bash
# 讓 git 使用 repo 裡的 .githooks/(pre-commit、commit-msg)。每個 clone 執行一次。
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
git config core.hooksPath .githooks
echo "git hooks installed (core.hooksPath = .githooks)"
if [[ ! -f .private-patterns && -z "${PRIVATE_PATTERNS:-}" ]]; then
  echo "note: no .private-patterns and no PRIVATE_PATTERNS; the scan will only look for secrets" >&2
fi
