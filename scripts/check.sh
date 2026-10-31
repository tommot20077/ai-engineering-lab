#!/usr/bin/env bash
# commit 前的全套檢查,跟 CI(.github/workflows/ci.yml)一樣:fmt、clippy、測試、供應鏈。
#
#   scripts/check.sh
#
# 先自動排版(cargo fmt),再用 --check 確認沒有遺漏;其他任何一項失敗就停下來。

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

step() { printf '\n== %s\n' "$*"; }

step "cargo fmt"
cargo fmt --all
cargo fmt --all --check

step "cargo clippy (-D warnings)"
cargo clippy --workspace --all-targets --quiet -- -D warnings
# 再用全部 feature 跑一次:`bench-internals` 底下的程式(llm::bench、benches/translate.rs)平常不會被編譯,
# 不檢查的話,改了內部函式它壞掉也沒人知道。
cargo clippy --workspace --all-targets --all-features --quiet -- -D warnings

step "cargo test (all features)"
cargo test --workspace --all-features --quiet

step "cargo deny"
if command -v cargo-deny >/dev/null; then
  cargo deny check --hide-inclusion-graph 2>&1 | tail -1
else
  echo "cargo-deny is not installed locally; CI will run it (cargo install cargo-deny)"
fi

step "private scan (all tracked files)"
scripts/private-scan.sh --all

printf '\nall checks passed\n'
