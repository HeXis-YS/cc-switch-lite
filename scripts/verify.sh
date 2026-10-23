#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p .codex/runs
run=".codex/runs/$(date -u +%Y%m%dT%H%M%S)-$$"
mkdir "$run"
exec > >(tee "$run/output.log") 2>&1
printf 'Run: %s\n' "$run"
rustc --version
cargo --version
git rev-parse --verify HEAD 2>/dev/null || printf 'Uncommitted initial repository\n'
git diff --binary > "$run/workspace.patch"
git status --short
find src tests -type f -print0 | sort -z | xargs -0 sha256sum > "$run/source-sha256.txt"
uname -a
sha256sum Cargo.toml Cargo.lock scripts/verify.sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
