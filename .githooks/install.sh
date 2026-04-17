#!/usr/bin/env bash
# Opt-in installer: point git at the repo's .githooks/ directory.
# Idempotent — safe to run multiple times.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

git config core.hooksPath .githooks
chmod +x .githooks/pre-commit .githooks/pre-push .githooks/install.sh

echo "hooks: core.hooksPath -> .githooks"

# Warn (don't block) on missing components. rust-toolchain.toml will
# auto-install on first cargo invocation anyway.
missing=()
command -v rustfmt >/dev/null 2>&1 || missing+=("rustfmt")
command -v cargo-clippy >/dev/null 2>&1 || missing+=("clippy")
if ((${#missing[@]})); then
    echo "hooks: warning — missing: ${missing[*]}"
    echo "hooks:          rustup component add ${missing[*]}"
fi

if ! command -v typos >/dev/null 2>&1; then
    echo "hooks: note — 'typos' not installed (optional)."
    echo "hooks:        cargo install typos-cli"
fi

echo "hooks: done. Bypass any hook with --no-verify."
