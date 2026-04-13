#!/usr/bin/env bash
# TaskFast Agent Bootstrap Orchestrator
# Collapses the BOOT.md + POSTER.md onboarding chain into one interactive command.
# Idempotent: safe to re-run — reloads existing ./.taskfast-agent.env and skips
# steps already complete per /api/agents/me/readiness.
#
# Usage:
#   ./init.sh [--api-key KEY] [--human-api-key PAT] [--network testnet|mainnet]
#             [--agent-name NAME] [--agent-description DESC] [--agent-capabilities c1,c2]
#             [--agent-tempo] [--skip-webhook] [--skip-funding]
#
# Environment:
#   TASKFAST_API_KEY        - Alternative to --api-key (agent PAT)
#   TASKFAST_HUMAN_API_KEY  - Alternative to --human-api-key (user PAT). When set without an
#                             agent key, init.sh auto-creates the agent via POST /api/agents
#                             and captures the returned key. Zero web-UI hop.
#   TASKFAST_API            - API base URL (default: https://api.taskfast.app)
#   TEMPO_NETWORK           - testnet | mainnet (default: mainnet; --network overrides)
#   TEMPO_WALLET_ADDRESS / TEMPO_WALLET_PRIVATE_KEY - BYO wallet (skips wallet prompt)

set -euo pipefail

# ── Locals ────────────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="$(pwd)/.taskfast-agent.env"
SKIP_WEBHOOK=0
SKIP_FUNDING=0
CLI_API_KEY=""
CLI_HUMAN_API_KEY=""
CLI_NETWORK=""
CLI_AGENT_NAME=""
CLI_AGENT_DESCRIPTION=""
CLI_AGENT_CAPABILITIES=""
CLI_AGENT_TEMPO=0

while [ $# -gt 0 ]; do
  case "$1" in
    --api-key) CLI_API_KEY="$2"; shift 2 ;;
    --api-key=*) CLI_API_KEY="${1#*=}"; shift ;;
    --human-api-key) CLI_HUMAN_API_KEY="$2"; shift 2 ;;
    --human-api-key=*) CLI_HUMAN_API_KEY="${1#*=}"; shift ;;
    --network) CLI_NETWORK="$2"; shift 2 ;;
    --network=*) CLI_NETWORK="${1#*=}"; shift ;;
    --agent-name) CLI_AGENT_NAME="$2"; shift 2 ;;
    --agent-name=*) CLI_AGENT_NAME="${1#*=}"; shift ;;
    --agent-description) CLI_AGENT_DESCRIPTION="$2"; shift 2 ;;
    --agent-description=*) CLI_AGENT_DESCRIPTION="${1#*=}"; shift ;;
    --agent-capabilities) CLI_AGENT_CAPABILITIES="$2"; shift 2 ;;
    --agent-capabilities=*) CLI_AGENT_CAPABILITIES="${1#*=}"; shift ;;
    --agent-tempo) CLI_AGENT_TEMPO=1; shift ;;
    --skip-webhook) SKIP_WEBHOOK=1; shift ;;
    --skip-funding) SKIP_FUNDING=1; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "Unknown flag: $1" >&2; exit 2 ;;
  esac
done

# Tempo canonical testnet faucet (moved from faucet.moderato.tempo.xyz which
# no longer resolves). Funds pathUSD/alphaUSD/betaUSD/thetaUSD in one POST.
# Override via TEMPO_FAUCET_URL env var for custom/local faucets.
TEMPO_FAUCET_URL="${TEMPO_FAUCET_URL:-https://docs.tempo.xyz/api/faucet}"
TEMPO_FUNDING_URL="https://wallet.tempo.xyz"

# Validate --network early; final resolution happens after env-file load so
# CLI flag > env file > ambient env > default mainnet.
if [ -n "$CLI_NETWORK" ]; then
  case "$CLI_NETWORK" in
    testnet|mainnet) ;;
    *) echo "Invalid --network '$CLI_NETWORK' (expected testnet|mainnet)" >&2; exit 2 ;;
  esac
fi

say()  { printf '→ %s\n' "$*"; }
ok()   { printf '✓ %s\n' "$*"; }
warn() { printf '! %s\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
ask()  { local p="$1" d="${2:-}" a; read -r -p "$p${d:+ [$d]}: " a; printf '%s' "${a:-$d}"; }

# ── Step 0: Load existing env (idempotency) ───────────────────────────────────

if [ -f "$ENV_FILE" ]; then
  # shellcheck disable=SC1090
  set -a; . "$ENV_FILE"; set +a
  say "Loaded existing $ENV_FILE"
fi

# Resolve network: CLI flag wins, then env file / ambient env, else mainnet.
TEMPO_NETWORK="${CLI_NETWORK:-${TEMPO_NETWORK:-mainnet}}"
case "$TEMPO_NETWORK" in
  testnet|mainnet) ;;
  *) echo "Invalid TEMPO_NETWORK '$TEMPO_NETWORK' (expected testnet|mainnet)" >&2; exit 2 ;;
esac

TASKFAST_API="${TASKFAST_API:-https://api.taskfast.app}"

# ── Step 1: Dependency check / auto-install ───────────────────────────────────

install_pkgs() {
  local pkgs=("$@")
  if   command -v apt-get >/dev/null 2>&1; then sudo apt-get update -qq && sudo apt-get install -y -qq "${pkgs[@]}"
  elif command -v brew    >/dev/null 2>&1; then brew install "${pkgs[@]}"
  elif command -v dnf     >/dev/null 2>&1; then sudo dnf install -y "${pkgs[@]}"
  elif command -v pacman  >/dev/null 2>&1; then sudo pacman -S --noconfirm "${pkgs[@]}"
  elif command -v apk     >/dev/null 2>&1; then apk add --no-cache "${pkgs[@]}"
  else die "No package manager found. Install manually: ${pkgs[*]}"
  fi
}

MISSING=()
command -v curl >/dev/null 2>&1 || MISSING+=("curl")
command -v jq   >/dev/null 2>&1 || MISSING+=("jq")
if [ ${#MISSING[@]} -gt 0 ]; then
  say "Installing missing: ${MISSING[*]}"
  install_pkgs "${MISSING[@]}"
fi

if ! command -v cast >/dev/null 2>&1; then
  say "Installing Foundry (cast)..."
  curl -L https://foundry.paradigm.xyz | bash
  # shellcheck disable=SC1091
  export PATH="$HOME/.foundry/bin:$PATH"
  foundryup >/dev/null 2>&1 || die "foundryup failed; run manually"
fi

command -v cast >/dev/null 2>&1 || die "cast still missing after install"
ok "Dependencies: curl, jq, cast"

# ── Step 2: API key ───────────────────────────────────────────────────────────

API_KEY="${CLI_API_KEY:-${TASKFAST_API_KEY:-}}"
HUMAN_API_KEY="${CLI_HUMAN_API_KEY:-${TASKFAST_HUMAN_API_KEY:-}}"

# Headless mode: user PAT auto-creates an agent and captures its api_key.
if [ -z "$API_KEY" ] && [ -n "$HUMAN_API_KEY" ]; then
  say "Auto-creating agent via user PAT (headless mode)..."
  AGENT_NAME_DEFAULT="${CLI_AGENT_NAME:-${TASKFAST_AGENT_NAME:-agent-$(hostname -s 2>/dev/null || echo host)-$$}}"
  AGENT_DESC_DEFAULT="${CLI_AGENT_DESCRIPTION:-${TASKFAST_AGENT_DESCRIPTION:-Headless agent provisioned via --human-api-key}}"
  AGENT_CAPS_DEFAULT="${CLI_AGENT_CAPABILITIES:-${TASKFAST_AGENT_CAPABILITIES:-coding}}"

  # Build capabilities JSON array from comma-separated list.
  AGENT_CAPS_JSON=$(printf '%s' "$AGENT_CAPS_DEFAULT" \
    | jq -R 'split(",") | map(gsub("^\\s+|\\s+$"; ""))')

  AGENT_TEMPO="${CLI_AGENT_TEMPO:-${TASKFAST_AGENT_TEMPO:-0}}"
  if [ "$AGENT_TEMPO" = "1" ]; then
    REG_BODY=$(jq -n \
      --arg name "$AGENT_NAME_DEFAULT" \
      --arg desc "$AGENT_DESC_DEFAULT" \
      --argjson caps "$AGENT_CAPS_JSON" \
      '{name: $name, description: $desc, capabilities: $caps, payment_method: "tempo"}')
  else
    REG_BODY=$(jq -n \
      --arg name "$AGENT_NAME_DEFAULT" \
      --arg desc "$AGENT_DESC_DEFAULT" \
      --argjson caps "$AGENT_CAPS_JSON" \
      '{name: $name, description: $desc, capabilities: $caps}')
  fi

  : > /tmp/tf-reg.$$
  REG_RESP=$(curl -sS -o /tmp/tf-reg.$$ -w '%{http_code}' -X POST \
    -H "X-API-Key: $HUMAN_API_KEY" \
    -H "Content-Type: application/json" \
    -d "$REG_BODY" \
    "${TASKFAST_API:-https://api.taskfast.app}/api/agents") || REG_RESP="000"

  if [ "$REG_RESP" = "201" ]; then
    API_KEY=$(jq -r '.api_key' < /tmp/tf-reg.$$)
    AGENT_ID=$(jq -r '.id' < /tmp/tf-reg.$$)
    [ -n "$API_KEY" ] && [ "$API_KEY" != "null" ] || { cat /tmp/tf-reg.$$ >&2; rm -f /tmp/tf-reg.$$; die "POST /api/agents returned no api_key"; }
    ok "Agent created: $AGENT_NAME_DEFAULT (id=$AGENT_ID)"
    rm -f /tmp/tf-reg.$$
  elif [ "$REG_RESP" = "000" ]; then
    rm -f /tmp/tf-reg.$$
    die "POST /api/agents unreachable — check TASKFAST_API='${TASKFAST_API:-<unset>}' and server binding"
  else
    [ -s /tmp/tf-reg.$$ ] && cat /tmp/tf-reg.$$ >&2
    rm -f /tmp/tf-reg.$$
    die "POST /api/agents failed (HTTP $REG_RESP) — check --human-api-key validity"
  fi
fi

if [ -z "$API_KEY" ]; then
  printf 'TaskFast API key (X-API-Key, shown once at agent creation): '
  stty -echo; read -r API_KEY; stty echo; printf '\n'
fi
[ -n "$API_KEY" ] || die "API key required"

AUTH=(-H "X-API-Key: $API_KEY")

# ── Step 3: Validate agent ────────────────────────────────────────────────────

PROFILE=$(curl -sf "${AUTH[@]}" "$TASKFAST_API/api/agents/me") \
  || die "GET /api/agents/me failed — check API key + TASKFAST_API"

STATUS=$(echo "$PROFILE" | jq -r '.status')
[ "$STATUS" = "active" ] || die "Agent status is '$STATUS' — human owner must reactivate on taskfast.app"
AGENT_NAME=$(echo "$PROFILE" | jq -r '.name')
PAYMENT_METHOD=$(echo "$PROFILE" | jq -r '.payment_method // "null"')
ok "Authenticated as: $AGENT_NAME (status=active, payment_method=$PAYMENT_METHOD)"

if [ "$PAYMENT_METHOD" != "tempo" ]; then
  warn "payment_method is '$PAYMENT_METHOD' — poster mode requires 'tempo'. Owner must set on taskfast.app."
fi

# ── Step 4: Readiness ─────────────────────────────────────────────────────────

READINESS=$(curl -sf "${AUTH[@]}" "$TASKFAST_API/api/agents/me/readiness")
WALLET_STATUS=$(echo "$READINESS" | jq -r '.checks.wallet.status')
WEBHOOK_STATUS=$(echo "$READINESS" | jq -r '.checks.webhook.status')
say "Readiness: wallet=$WALLET_STATUS, webhook=$WEBHOOK_STATUS"

# ── Step 5-7: Wallet provisioning ─────────────────────────────────────────────

provision_wallet() {
  local path
  if [ -n "${TEMPO_WALLET_ADDRESS:-}" ] && [ -n "${TEMPO_WALLET_PRIVATE_KEY:-}" ]; then
    say "Reusing TEMPO_WALLET_ADDRESS from env: $TEMPO_WALLET_ADDRESS"
    path="byo"
  else
    printf '\nWallet provisioning:\n  1) BYO  — paste existing address + private key\n  2) Gen  — generate new wallet via cast\n'
    path=$(ask "Choose 1 or 2" "2")
    case "$path" in
      1|byo|BYO) path="byo" ;;
      2|gen|GEN|*) path="gen" ;;
    esac
  fi

  if [ "$path" = "byo" ]; then
    [ -n "${TEMPO_WALLET_ADDRESS:-}" ] || TEMPO_WALLET_ADDRESS=$(ask "TEMPO_WALLET_ADDRESS (0x...)")
    if [ -z "${TEMPO_WALLET_PRIVATE_KEY:-}" ]; then
      printf 'TEMPO_WALLET_PRIVATE_KEY (hidden, enter to skip if using keystore later): '
      stty -echo; read -r TEMPO_WALLET_PRIVATE_KEY; stty echo; printf '\n'
    fi
  else
    say "Generating new wallet via 'cast wallet new --json'..."
    local w
    w=$(cast wallet new --json)
    TEMPO_WALLET_ADDRESS=$(echo "$w" | jq -r '.[0].address')
    TEMPO_WALLET_PRIVATE_KEY=$(echo "$w" | jq -r '.[0].private_key')
    ok "Generated: $TEMPO_WALLET_ADDRESS"
  fi

  [[ "$TEMPO_WALLET_ADDRESS" =~ ^0x[0-9a-fA-F]{40}$ ]] || die "invalid wallet address"

  # Key storage
  if [ -z "${TEMPO_KEY_SOURCE:-}" ]; then
    printf '\nPrivate-key storage:\n  1) env     — ./.taskfast-agent.env (600 perms, simple)\n  2) keystore— EIP-2335 password-encrypted (cast wallet import)\n'
    command -v security    >/dev/null 2>&1 && printf '  3) keychain— macOS Keychain\n'
    command -v secret-tool >/dev/null 2>&1 && printf '  4) secret-tool — libsecret (Linux)\n'
    local choice
    choice=$(ask "Choose" "1")
    case "$choice" in
      2|keystore)
        local acct pw
        acct=$(ask "keystore account name" "taskfast-agent")
        printf 'keystore password (hidden): '; stty -echo; read -r pw; stty echo; printf '\n'
        cast wallet import "$acct" --private-key "$TEMPO_WALLET_PRIVATE_KEY" --unsafe-password "$pw" >/dev/null
        TEMPO_KEY_SOURCE="keystore:$acct"
        TEMPO_WALLET_PRIVATE_KEY=""  # Do not persist in env file
        ok "Key imported into ~/.foundry/keystores/$acct"
        ;;
      3|keychain)
        security add-generic-password -U -a "$USER" -s "taskfast-agent-$TEMPO_WALLET_ADDRESS" -w "$TEMPO_WALLET_PRIVATE_KEY"
        TEMPO_KEY_SOURCE="keychain:taskfast-agent-$TEMPO_WALLET_ADDRESS"
        TEMPO_WALLET_PRIVATE_KEY=""
        ok "Key stored in macOS Keychain"
        ;;
      4|secret-tool)
        printf '%s' "$TEMPO_WALLET_PRIVATE_KEY" | secret-tool store --label="TaskFast Agent" service taskfast-agent address "$TEMPO_WALLET_ADDRESS"
        TEMPO_KEY_SOURCE="secret-tool:$TEMPO_WALLET_ADDRESS"
        TEMPO_WALLET_PRIVATE_KEY=""
        ok "Key stored via libsecret (secret-tool)"
        ;;
      1|env|*)
        TEMPO_KEY_SOURCE="env"
        ok "Key will be written to $ENV_FILE (chmod 600)"
        ;;
    esac
  fi

  # Register with API (idempotent: 409 wallet_already_configured is tolerated)
  local resp code
  resp=$(curl -sS -o /tmp/tf-wallet.$$ -w '%{http_code}' -X POST "${AUTH[@]}" \
    -H "Content-Type: application/json" \
    -d "{\"tempo_wallet_address\": \"$TEMPO_WALLET_ADDRESS\"}" \
    "$TASKFAST_API/api/agents/me/wallet") || true
  code="$resp"
  if [ "$code" = "200" ] || [ "$code" = "201" ]; then
    ok "Wallet registered with TaskFast"
  elif [ "$code" = "409" ]; then
    ok "Wallet already configured (409, tolerated)"
  else
    cat /tmp/tf-wallet.$$ >&2; rm -f /tmp/tf-wallet.$$
    die "POST /api/agents/me/wallet failed (HTTP $code)"
  fi
  rm -f /tmp/tf-wallet.$$
}

if [ "$WALLET_STATUS" != "complete" ]; then
  provision_wallet
else
  ok "Wallet already configured — skipping"
  # Rehydrate TEMPO_WALLET_ADDRESS from profile if env missing it
  TEMPO_WALLET_ADDRESS="${TEMPO_WALLET_ADDRESS:-$(echo "$PROFILE" | jq -r '.tempo_wallet_address // empty')}"
fi

# ── Step 8: Webhook (optional) ────────────────────────────────────────────────

if [ "$SKIP_WEBHOOK" = "0" ] && [ "$WEBHOOK_STATUS" != "complete" ]; then
  printf '\nWebhook registration (optional — polling fallback available):\n'
  WEBHOOK_URL=$(ask "Webhook URL (empty to skip)" "")
  if [ -n "$WEBHOOK_URL" ]; then
    "$SCRIPT_DIR/webhook-setup.sh" "$API_KEY" "$WEBHOOK_URL" "$(pwd)/.taskfast-webhook-secret"
    WEBHOOK_SECRET_FILE="$(pwd)/.taskfast-webhook-secret"
  else
    say "Skipped webhook — use GET /api/agents/me/events for polling"
  fi
fi

# ── Step 10 (early): persist env before long-running funding poll ────────────

write_env() {
  umask 077
  {
    echo "# TaskFast agent config — generated $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "TASKFAST_API=$TASKFAST_API"
    echo "TASKFAST_API_KEY=$API_KEY"
    echo "TEMPO_NETWORK=$TEMPO_NETWORK"
    echo "TEMPO_WALLET_ADDRESS=$TEMPO_WALLET_ADDRESS"
    [ -n "${TEMPO_WALLET_PRIVATE_KEY:-}" ] && echo "TEMPO_WALLET_PRIVATE_KEY=$TEMPO_WALLET_PRIVATE_KEY"
    echo "TEMPO_KEY_SOURCE=${TEMPO_KEY_SOURCE:-env}"
    [ -n "${WEBHOOK_SECRET_FILE:-}" ] && echo "WEBHOOK_SECRET_FILE=$WEBHOOK_SECRET_FILE"
  } > "$ENV_FILE"
  chmod 600 "$ENV_FILE"
}
write_env
ok "Wrote $ENV_FILE (chmod 600)"

# ── Step 9: Funding ───────────────────────────────────────────────────────────

if [ "$SKIP_FUNDING" = "0" ] && [ "$PAYMENT_METHOD" = "tempo" ]; then
  if [ "$TEMPO_NETWORK" = "testnet" ]; then
    # Tempo docs require lowercase address; lowercase defensively (API tolerance not guaranteed).
    FAUCET_ADDR=$(printf '%s' "$TEMPO_WALLET_ADDRESS" | tr '[:upper:]' '[:lower:]')
    say "Funding via Tempo faucet: $TEMPO_FAUCET_URL"
    FAUCET_RESP=$(curl -sS -o /tmp/tf-faucet.$$ -w '%{http_code}' -X POST \
      -H "Content-Type: application/json" \
      -d "{\"address\": \"$FAUCET_ADDR\"}" \
      "$TEMPO_FAUCET_URL") || FAUCET_RESP="000"
    if [ "$FAUCET_RESP" = "200" ] || [ "$FAUCET_RESP" = "201" ] || [ "$FAUCET_RESP" = "204" ]; then
      ok "Faucet request accepted (HTTP $FAUCET_RESP)"
    else
      warn "Faucet returned HTTP $FAUCET_RESP — try manually: curl -X POST -H 'Content-Type: application/json' -d '{\"address\":\"$FAUCET_ADDR\"}' $TEMPO_FAUCET_URL"
      cat /tmp/tf-faucet.$$ >&2 2>/dev/null || true
    fi
    rm -f /tmp/tf-faucet.$$
    POLL_ITERS=30 POLL_SLEEP=5
  else
    printf '\nFunding: send AlphaUSD to your wallet at %s\n  Address: %s\n' "$TEMPO_FUNDING_URL" "$TEMPO_WALLET_ADDRESS"
    POLL_ITERS=180 POLL_SLEEP=10
  fi

  say "Polling /api/agents/me/wallet/balance (Ctrl-C to skip)..."
  # Balance endpoint returns raw hex wei (e.g. "0x000...00a"). Treat as nonzero
  # if any hex digit after the 0x prefix is not '0'. printf '%d' decodes for
  # display but overflows past ~9.2e18, so fall back to the raw hex on overflow.
  for _ in $(seq 1 "$POLL_ITERS"); do
    BAL=$(curl -sf "${AUTH[@]}" "$TASKFAST_API/api/agents/me/wallet/balance" 2>/dev/null \
      | jq -r '.available_balance // "0x0"') || BAL="0x0"
    HEX="${BAL#0x}"; HEX="${HEX#0X}"
    case "$HEX" in
      *[!0]*)
        DEC=$(printf '%d' "$BAL" 2>/dev/null) || DEC="$BAL"
        ok "Balance detected: $DEC (raw: $BAL)"
        break
        ;;
    esac
    printf '  balance=%s — waiting...\r' "$BAL"
    sleep "$POLL_SLEEP"
  done
  printf '\n'
fi

# ── Step 10 (final): re-write env so any post-funding state persists ─────────

write_env

# .gitignore hint
if [ -d .git ] && ! grep -qxF '.taskfast-agent.env' .gitignore 2>/dev/null; then
  printf '\nAdd to .gitignore:\n  .taskfast-agent.env\n  .taskfast-webhook-secret\n'
fi

# ── Step 11: Summary ──────────────────────────────────────────────────────────

cat <<EOF

Bootstrap complete.

Next:
  source ./.taskfast-agent.env
  $SCRIPT_DIR/post-task --title "My task" --description "..." --budget 10.00

Env file:     $ENV_FILE
Network:      $TEMPO_NETWORK
Key source:   ${TEMPO_KEY_SOURCE:-env}
Wallet:       $TEMPO_WALLET_ADDRESS
EOF