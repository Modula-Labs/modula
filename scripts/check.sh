#!/usr/bin/env bash
# Everything CI runs, runnable locally. Needs Postgres (DATABASE_URL) and,
# optionally, Redis (MODULA_TEST_REDIS_URL); `scripts/dev-services.sh` starts both.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${DATABASE_URL:=postgres://modula:modula@127.0.0.1:55432/modula}"
: "${MODULA_TEST_REDIS_URL:=redis://:modula@127.0.0.1:56379}"
export DATABASE_URL MODULA_TEST_REDIS_URL

scripts/no-shell.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
if command -v cargo-deny >/dev/null; then cargo deny check; else echo "skip: cargo-deny not installed"; fi
if command -v cargo-audit >/dev/null; then cargo audit; else echo "skip: cargo-audit not installed"; fi
