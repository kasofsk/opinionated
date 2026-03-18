#!/usr/bin/env bash
# workers.sh — create Forgejo tokens for workers and launch worker containers.
#
# Prerequisites: ./scripts/init.sh must have run (Forgejo + sidecar up).
#
# Usage:
#   ./scripts/workers.sh              # start all 3 workers
#   ./scripts/workers.sh --count 2    # start first 2 workers
#   ./scripts/workers.sh --down       # stop worker containers
#   ./scripts/workers.sh --build      # rebuild worker image before starting

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

FORGEJO_URL="${FORGEJO_URL:-http://localhost:3000}"
WORKER_PASS="${WORKER_PASS:-worker-test-1234}"
DELAY_SECS="${DELAY_SECS:-10}"

# All provisioned workers (must match Terraform worker_logins)
ALL_WORKERS=(worker-aria worker-blake worker-casey)

# Parse args
COUNT=${#ALL_WORKERS[@]}
ACTION="up"
BUILD_FLAG=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --count)  COUNT="$2"; shift 2 ;;
        --down)   ACTION="down"; shift ;;
        --build)  BUILD_FLAG="--build"; shift ;;
        --delay)  DELAY_SECS="$2"; shift 2 ;;
        *)        echo "Unknown arg: $1"; exit 1 ;;
    esac
done

cd "$ROOT"

if [[ "$ACTION" == "down" ]]; then
    docker compose -f docker-compose.workers.yml down
    rm -f .workers.env
    exit 0
fi

# ── Create API tokens for each worker ────────────────────────────────────────

SELECTED=("${ALL_WORKERS[@]:0:$COUNT}")
SERVICES=()

echo "Creating Forgejo API tokens for ${#SELECTED[@]} workers ..."

: > .workers.env   # truncate

for login in "${SELECTED[@]}"; do
    TOKEN_NAME="dispatch"

    # Delete existing token (ignore errors)
    curl -sf -X DELETE \
        -u "$login:$WORKER_PASS" \
        "$FORGEJO_URL/api/v1/users/$login/tokens/$TOKEN_NAME" \
        > /dev/null 2>&1 || true

    # Create fresh token
    resp=$(curl -sf -X POST \
        -u "$login:$WORKER_PASS" \
        -H "Content-Type: application/json" \
        -d "{\"name\":\"$TOKEN_NAME\",\"scopes\":[\"write:issue\",\"read:issue\",\"write:repository\",\"read:repository\"]}" \
        "$FORGEJO_URL/api/v1/users/$login/tokens")

    token=$(echo "$resp" | grep -o '"sha1":"[^"]*"' | cut -d'"' -f4)
    if [[ -z "$token" ]]; then
        echo "  ❌ Failed to create token for $login: $resp"
        exit 1
    fi

    # Write env var: worker-aria → WORKER_ARIA_TOKEN
    var_name="WORKER_$(echo "${login#worker-}" | tr '[:lower:]' '[:upper:]')_TOKEN"
    echo "$var_name=$token" >> .workers.env
    echo "  ✓ $login → $var_name"

    SERVICES+=("$login")
done

# ── Launch worker containers ─────────────────────────────────────────────────

echo ""
echo "Starting ${#SERVICES[@]} worker containers (delay=${DELAY_SECS}s) ..."

env $(cat .workers.env | xargs) \
    docker compose -f docker-compose.workers.yml up -d $BUILD_FLAG "${SERVICES[@]}"

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Workers running:"
for s in "${SERVICES[@]}"; do
    echo "    $s"
done
echo ""
echo "  Logs:  docker compose -f docker-compose.workers.yml logs -f"
echo "  Stop:  ./scripts/workers.sh --down"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
