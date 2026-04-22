#!/usr/bin/env bash
# Opt-in installer: point git at the repo's .githooks/ directory.
# Idempotent — safe to run multiple times.
#
# Note: .githooks/* chain to beads via `bd hooks run` so `bd init` is
# not required after running this. If you later run `bd hooks install`,
# it will repoint core.hooksPath back to .beads/hooks and skip repo
# gates — re-run this script to restore.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

git config core.hooksPath .githooks
chmod +x \
    .githooks/pre-commit \
    .githooks/commit-msg \
    .githooks/prepare-commit-msg \
    .githooks/pre-push \
    .githooks/post-merge \
    .githooks/post-checkout \
    .githooks/install.sh

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

if ! command -v gitleaks >/dev/null 2>&1; then
    echo "hooks: note — 'gitleaks' not installed (optional, pre-commit secret scan)."
    echo "hooks:        brew install gitleaks  # or: apt install gitleaks"
fi

# Only warn about audit/deny when the user opts into the full gate.
if [[ "${TASKFAST_FULL_GATE:-0}" == "1" ]]; then
    for bin in cargo-audit cargo-deny cargo-machete; do
        if ! command -v "$bin" >/dev/null 2>&1; then
            echo "hooks: note — '$bin' not installed (TASKFAST_FULL_GATE=1)."
            echo "hooks:        cargo install $bin"
        fi
    done
fi

if ! command -v bd >/dev/null 2>&1; then
    echo "hooks: note — 'bd' not installed; beads chain in hooks will be skipped."
fi

echo "hooks: done. Bypass any hook with --no-verify."
