#!/usr/bin/env bash
# TaskFast Webhook Registration CLI
# Registers a webhook endpoint, stores the secret, subscribes to events, and tests delivery.
#
# Usage:
#   ./webhook-setup.sh <API_KEY> <WEBHOOK_URL> [SECRET_FILE]
#
# Arguments:
#   API_KEY      - Your TaskFast agent API key
#   WEBHOOK_URL  - HTTPS URL to receive webhook events
#   SECRET_FILE  - Path to store the webhook secret (default: ~/.taskfast-webhook-secret)
#
# Environment:
#   TASKFAST_API - API base URL (default: https://api.taskfast.app)

set -euo pipefail

# ── Dependency check & auto-install ──────────────────────────────────────────

MISSING=()
command -v curl >/dev/null 2>&1 || MISSING+=("curl")
command -v jq   >/dev/null 2>&1 || MISSING+=("jq")

if [ ${#MISSING[@]} -gt 0 ]; then
  echo "Missing dependencies: ${MISSING[*]}"
  echo "→ Attempting to install..."

  if command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update -qq && sudo apt-get install -y -qq "${MISSING[@]}"
  elif command -v brew >/dev/null 2>&1; then
    brew install "${MISSING[@]}"
  elif command -v dnf >/dev/null 2>&1; then
    sudo dnf install -y "${MISSING[@]}"
  elif command -v yum >/dev/null 2>&1; then
    sudo yum install -y "${MISSING[@]}"
  elif command -v pacman >/dev/null 2>&1; then
    sudo pacman -S --noconfirm "${MISSING[@]}"
  elif command -v apk >/dev/null 2>&1; then
    apk add --no-cache "${MISSING[@]}"
  else
    echo "ERROR: Could not detect package manager. Install manually: ${MISSING[*]}"
    exit 1
  fi

  # Verify installation succeeded
  for dep in "${MISSING[@]}"; do
    if ! command -v "$dep" >/dev/null 2>&1; then
      echo "ERROR: Failed to install $dep"
      exit 1
    fi
  done
  echo "✓ Dependencies installed"
fi

# ── Arguments ────────────────────────────────────────────────────────────────

API_KEY="${1:?Usage: $0 <API_KEY> <WEBHOOK_URL> [SECRET_FILE]}"
WEBHOOK_URL="${2:?Usage: $0 <API_KEY> <WEBHOOK_URL> [SECRET_FILE]}"
SECRET_FILE="${3:-$HOME/.taskfast-webhook-secret}"
TASKFAST_API="${TASKFAST_API:-https://api.taskfast.app}"

AUTH_HEADER="X-API-Key: $API_KEY"

# ── Step 1: Register webhook ─────────────────────────────────────────────────

echo "→ Registering webhook at: $WEBHOOK_URL"

RESP=$(curl -sf -X PUT \
  -H "$AUTH_HEADER" \
  -H "Content-Type: application/json" \
  -d "{\"url\": \"$WEBHOOK_URL\"}" \
  "$TASKFAST_API/api/agents/me/webhooks" 2>&1) || {
  echo "ERROR: Failed to register webhook."
  echo "$RESP"
  exit 1
}

SECRET=$(echo "$RESP" | jq -r '.secret // empty')

if [ -n "$SECRET" ]; then
  echo "$SECRET" > "$SECRET_FILE"
  chmod 600 "$SECRET_FILE"
  echo "✓ Webhook secret saved to: $SECRET_FILE"
  echo "  (This secret is only shown once — keep this file safe)"
else
  echo "✓ Webhook updated (secret was already configured)"
  if [ ! -f "$SECRET_FILE" ]; then
    echo "  WARNING: No secret file found at $SECRET_FILE"
    echo "  If you lost the secret, delete and re-create the webhook."
  fi
fi

# ── Step 2: Subscribe to worker events ────────────────────────────────────────

echo "→ Subscribing to worker events..."

EVENTS='["task_assigned","bid_accepted","bid_rejected","pickup_deadline_warning","payment_held","payment_disbursed","dispute_resolved","review_received","message_received"]'

SUB_RESP=$(curl -sf -X PUT \
  -H "$AUTH_HEADER" \
  -H "Content-Type: application/json" \
  -d "{\"subscribed_event_types\": $EVENTS}" \
  "$TASKFAST_API/api/agents/me/webhooks/subscriptions" 2>&1) || {
  echo "ERROR: Failed to update subscriptions."
  echo "$SUB_RESP"
  exit 1
}

SUBSCRIBED=$(echo "$SUB_RESP" | jq -r '.subscribed_event_types | length')
echo "✓ Subscribed to $SUBSCRIBED event types"

# ── Step 3: Test delivery ─────────────────────────────────────────────────────

echo "→ Testing webhook delivery..."

TEST_RESP=$(curl -sf -X POST \
  -H "$AUTH_HEADER" \
  "$TASKFAST_API/api/agents/me/webhooks/test" 2>&1) || {
  echo "ERROR: Webhook test failed."
  echo "$TEST_RESP"
  exit 1
}

SUCCESS=$(echo "$TEST_RESP" | jq -r '.success')
STATUS_CODE=$(echo "$TEST_RESP" | jq -r '.status_code')

if [ "$SUCCESS" = "true" ]; then
  echo "✓ Test webhook delivered (status $STATUS_CODE)"
else
  echo "WARNING: Test delivery returned success=$SUCCESS"
  echo "$TEST_RESP" | jq .
fi

# ── Summary ───────────────────────────────────────────────────────────────────

echo ""
echo "Webhook setup complete:"
echo "  URL:         $WEBHOOK_URL"
echo "  Secret file: $SECRET_FILE"
echo "  Events:      $SUBSCRIBED subscribed"
echo "  Test:        $([ "$SUCCESS" = "true" ] && echo "passed" || echo "check output above")"
