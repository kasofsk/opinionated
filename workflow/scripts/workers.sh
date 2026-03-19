#!/usr/bin/env bash
# workers.sh — create Forgejo tokens for workers and launch worker containers.
#
# Prerequisites: ./scripts/init.sh must have run (Forgejo + sidecar up).
#
# Usage:
#   ./scripts/workers.sh              # start action workers + paired runners (default)
#   ./scripts/workers.sh --sim        # start sim workers (no runners needed)
#   ./scripts/workers.sh --count 2    # start first 2 workers only
#   ./scripts/workers.sh --down       # stop all worker/runner containers
#   ./scripts/workers.sh --build      # rebuild worker image before starting

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

FORGEJO_URL="${FORGEJO_URL:-http://localhost:3000}"
WORKER_PASS="${WORKER_PASS:-worker-test-1234}"
ADMIN_USER="${ADMIN_USER:-sysadmin}"
ADMIN_PASS="${ADMIN_PASS:-admin1234}"
DELAY_SECS="${DELAY_SECS:-10}"

# All provisioned workers (must match Terraform worker_logins)
ALL_WORKERS=(worker-aria worker-blake worker-casey)

# Parse args
COUNT=${#ALL_WORKERS[@]}
ACTION="up"
MODE="action"
BUILD_FLAG=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --count)   COUNT="$2"; shift 2 ;;
        --down)    ACTION="down"; shift ;;
        --build)   BUILD_FLAG="--build"; shift ;;
        --delay)   DELAY_SECS="$2"; shift 2 ;;
        --sim)     MODE="sim"; shift ;;
        *)         echo "Unknown arg: $1"; exit 1 ;;
    esac
done

cd "$ROOT"

if [[ "$ACTION" == "down" ]]; then
    docker compose -f docker-compose.workers.yml --profile sim --profile action down
    rm -f .workers.env
    exit 0
fi

# ── Create API tokens for each worker ────────────────────────────────────────

SELECTED=("${ALL_WORKERS[@]:0:$COUNT}")

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
done

# ── Action mode: set up runners + push workflow ──────────────────────────────

if [[ "$MODE" == "action" ]]; then
    echo ""
    echo "Setting up action runners ..."

    # Get a runner registration token from Forgejo admin API.
    REG_TOKEN=$(curl -sf \
        -H "Authorization: token $(grep -o 'FORGEJO_TOKEN=.*' .sidecar.env | cut -d= -f2-)" \
        "$FORGEJO_URL/api/v1/admin/runners/registration-token" 2>/dev/null \
        | grep -o '"token":"[^"]*"' | cut -d'"' -f4 || true)

    # Fallback: use admin basic auth
    if [[ -z "$REG_TOKEN" ]]; then
        REG_TOKEN=$(curl -sf -X GET \
            -u "$ADMIN_USER:$ADMIN_PASS" \
            "$FORGEJO_URL/api/v1/admin/runners/registration-token" \
            | grep -o '"token":"[^"]*"' | cut -d'"' -f4 || true)
    fi

    if [[ -z "$REG_TOKEN" ]]; then
        echo "  ❌ Failed to get runner registration token from Forgejo."
        echo "     Make sure Actions is enabled in app.ini and Forgejo is running."
        exit 1
    fi

    echo "RUNNER_REGISTRATION_TOKEN=$REG_TOKEN" >> .workers.env
    echo "  ✓ Runner registration token obtained"

fi

# ── Launch containers ────────────────────────────────────────────────────────

echo ""

if [[ "$MODE" == "action" ]]; then
    # Build the list of action + runner services
    SERVICES=()
    for login in "${SELECTED[@]}"; do
        name="${login#worker-}"
        SERVICES+=("action-$name" "runner-$name")
    done

    echo "Starting ${#SELECTED[@]} action workers + runners ..."
    env $(cat .workers.env | xargs) \
        docker compose -f docker-compose.workers.yml --profile action up -d $BUILD_FLAG "${SERVICES[@]}"
else
    # Sim mode
    SERVICES=("${SELECTED[@]}")

    echo "Starting ${#SERVICES[@]} sim workers (delay=${DELAY_SECS}s) ..."
    env $(cat .workers.env | xargs) \
        docker compose -f docker-compose.workers.yml --profile sim up -d $BUILD_FLAG "${SERVICES[@]}"
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Workers running ($MODE mode):"
for s in "${SERVICES[@]}"; do
    echo "    $s"
done
echo ""
echo "  Logs:  docker compose -f docker-compose.workers.yml logs -f"
echo "  Stop:  ./scripts/workers.sh --down"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
