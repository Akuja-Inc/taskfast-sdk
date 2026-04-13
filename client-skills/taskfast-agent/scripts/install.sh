#!/usr/bin/env bash
# TaskFast Agent Bootstrap — remote installer (curl | bash wrapper)
#
# Usage (interactive):
#   curl -fsSL https://raw.githubusercontent.com/Akuja-Inc/taskfast-sdk/main/client-skills/taskfast-agent/scripts/install.sh | bash
#
# Usage (with args — note the `-s --` for arg forwarding):
#   curl -fsSL https://raw.githubusercontent.com/Akuja-Inc/taskfast-sdk/main/client-skills/taskfast-agent/scripts/install.sh \
#     | bash -s -- --api-key KEY --skip-webhook
#
# What this does:
#   1. Downloads init.sh + its sibling scripts (webhook-setup.sh, post-task)
#      from a pinned commit of github.com/Akuja-Inc/taskfast-sdk.
#   2. Verifies SHA-256 of every file against checksums embedded below.
#      Mismatch = hard fail (no execution).
#   3. Execs init.sh with any args forwarded, so $SCRIPT_DIR resolves to a
#      dir that actually contains webhook-setup.sh.
#
# Bumping the pin:
#   - Update TASKFAST_INIT_REF to the new commit SHA.
#   - Recompute all three checksums from that commit:
#       sha256sum client-skills/taskfast-agent/scripts/{init.sh,webhook-setup.sh,post-task}
#   - Update the three TASKFAST_*_SHA256 values below.
#   - Also update the example URLs in this header.
#
# TODO(am-5y9): TASKFAST_INIT_REF currently tracks `main` because the relocation
# commit (skill move from Akuja-Inc/agent-marketplace) hasn't landed in a tagged
# release yet. Pin to a real SHA once a release exists. The plan's end-state
# replaces this entire shell installer with the `taskfast` Rust binary
# distributed via https://taskfast.app/install.sh.

set -euo pipefail

# ── Pinned release (override via env for local testing) ─────────────────────

TASKFAST_INIT_REPO="${TASKFAST_INIT_REPO:-Akuja-Inc/taskfast-sdk}"
TASKFAST_INIT_REF="${TASKFAST_INIT_REF:-main}"
TASKFAST_INIT_BASE="${TASKFAST_INIT_BASE:-https://raw.githubusercontent.com/$TASKFAST_INIT_REPO/$TASKFAST_INIT_REF/client-skills/taskfast-agent/scripts}"

TASKFAST_INIT_SHA256="${TASKFAST_INIT_SHA256:-5bef96a522a2c5db16824a9172aa7fdc64f38973636d4482e20137769b874fad}"
TASKFAST_WEBHOOK_SHA256="${TASKFAST_WEBHOOK_SHA256:-2cae2c3f473d24112376948a283dbd76e700c24bc22f6b2350fe488fc5500680}"
TASKFAST_POSTTASK_SHA256="${TASKFAST_POSTTASK_SHA256:-913769da98b07d077c3d6e11e9c426be7e541ba5cb36a6c6785a0e3be52b700a}"

# ── Deps ────────────────────────────────────────────────────────────────────

command -v curl >/dev/null 2>&1 || { echo "ERROR: curl required to bootstrap" >&2; exit 1; }

if   command -v sha256sum >/dev/null 2>&1; then HASHER="sha256sum"
elif command -v shasum    >/dev/null 2>&1; then HASHER="shasum -a 256"
else echo "ERROR: need sha256sum or shasum for checksum verification" >&2; exit 1
fi

# ── Staging dir ─────────────────────────────────────────────────────────────

STAGE="$(mktemp -d -t taskfast-install.XXXXXX)"
trap 'rm -rf "$STAGE"' EXIT

fetch_verify() {
  local name="$1" expected="$2" url="$TASKFAST_INIT_BASE/$1" dest="$STAGE/$1"
  echo "→ Fetching $name"
  curl -fsSL "$url" -o "$dest" || { echo "ERROR: download failed: $url" >&2; exit 1; }
  local got
  got=$($HASHER "$dest" | awk '{print $1}')
  if [ "$got" != "$expected" ]; then
    echo "ERROR: SHA-256 mismatch for $name" >&2
    echo "  expected: $expected" >&2
    echo "  got:      $got" >&2
    echo "  url:      $url" >&2
    echo "  ref:      $TASKFAST_INIT_REF" >&2
    echo "Refusing to execute. If you updated the pin, recompute checksums." >&2
    exit 1
  fi
  echo "✓ Verified $name ($expected)"
}

fetch_verify "init.sh"         "$TASKFAST_INIT_SHA256"
fetch_verify "webhook-setup.sh" "$TASKFAST_WEBHOOK_SHA256"
fetch_verify "post-task"       "$TASKFAST_POSTTASK_SHA256"

chmod +x "$STAGE/init.sh" "$STAGE/webhook-setup.sh" "$STAGE/post-task"

# ── Exec init.sh with forwarded args ────────────────────────────────────────

exec bash "$STAGE/init.sh" "$@"
