#!/usr/bin/env bash
# init.sh — sets up (or resets) the entire workflow dev/test environment.
#
# Idempotent: always tears down everything and rebuilds from scratch.
#
# What it does:
#   1. Stops any running sidecar/CDC processes
#   2. Tears down Docker containers, clears local state
#   3. Starts Forgejo + NATS via docker compose
#   4. Creates the admin user + API token
#   5. Runs Terraform (destroy + apply) for repos, users, labels, webhooks
#   6. Sets worker and sidecar passwords, creates sidecar API token
#   7. Writes .sidecar.env
#   8. Builds all binaries
#   9. Starts CDC + sidecar
#  10. Seeds a fixture and verifies the graph
#
# Usage:
#   ./scripts/init.sh                       # full setup with default hub fixture
#   ./scripts/init.sh --no-seed             # skip fixture seeding
#   ./scripts/init.sh --fixture chain.json  # seed a different fixture
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

FORGEJO_URL="${FORGEJO_URL:-http://localhost:3000}"
ADMIN_USER="${ADMIN_USER:-sysadmin}"
ADMIN_PASS="${ADMIN_PASS:-admin1234}"
ADMIN_EMAIL="${ADMIN_EMAIL:-sysadmin@test.local}"
ADMIN_TOKEN_NAME="${ADMIN_TOKEN_NAME:-terraform}"

SIDECAR_LOGIN="${SIDECAR_LOGIN:-workflow-sync}"
SIDECAR_PASS="${SIDECAR_PASS:-sidecar-test-1234}"
SIDECAR_TOKEN_NAME="workflow-sync"
DISPATCHER_LOGIN="${DISPATCHER_LOGIN:-workflow-dispatcher}"
DISPATCHER_PASS="${DISPATCHER_PASS:-dispatcher-test-1234}"
DISPATCHER_TOKEN_NAME="workflow-dispatcher"
REVIEWER_LOGIN="${REVIEWER_LOGIN:-workflow-reviewer}"
REVIEWER_PASS="${REVIEWER_PASS:-reviewer-test-1234}"
REVIEWER_TOKEN_NAME="workflow-reviewer"
HUMAN_LOGIN="${HUMAN_LOGIN:-you}"
HUMAN_PASS="${HUMAN_PASS:-human-test-1234}"
WORKER_PASS="${WORKER_PASS:-worker-test-1234}"

# Parse args
SEED=true
FIXTURE="demo/fixtures/hub.json"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-seed)       SEED=false; shift ;;
        --fixture)       FIXTURE="$2"; shift 2 ;;
        *)               echo "Unknown arg: $1"; exit 1 ;;
    esac
done

cd "$ROOT"

# ── 1. Kill running processes ────────────────────────────────────────────────

echo "🧹 Cleaning up running processes ..."
pkill -f "target/debug/workflow-sidecar" 2>/dev/null || true
pkill -f "target/debug/workflow-cdc" 2>/dev/null || true
sleep 1
echo "✓ Processes stopped"

# ── 2. Tear down infra and clear state ───────────────────────────────────────

echo "🗑  Tearing down containers and clearing state ..."
docker compose down -v 2>/dev/null || true
docker compose -f docker-compose.workers.yml --profile sim --profile action down -v 2>/dev/null || true
rm -rf .data/forgejo .data/workflow.db
mkdir -p .data/forgejo/gitea/conf
cp infra/forgejo/app.ini .data/forgejo/gitea/conf/app.ini
echo "✓ Clean slate"

# ── 3. Start infra ──────────────────────────────────────────────────────────

echo "🐳 Starting infra (forgejo + nats) ..."
docker compose up -d forgejo nats
echo "✓ Containers started"

# ── 4. Wait for Forgejo ─────────────────────────────────────────────────────

echo "⏳ Waiting for Forgejo at $FORGEJO_URL ..."
for i in $(seq 1 60); do
    curl -sf "$FORGEJO_URL/api/v1/version" > /dev/null 2>&1 && break
    sleep 2
done
curl -sf "$FORGEJO_URL/api/v1/version" > /dev/null 2>&1 || { echo "❌ Forgejo did not start"; exit 1; }
echo "✓ Forgejo is up"

# ── 5. Create admin user (idempotent) ───────────────────────────────────────

echo "👤 Creating admin user '$ADMIN_USER' ..."
docker compose exec -T -u git forgejo \
    gitea admin user create \
    --username "$ADMIN_USER" \
    --password "$ADMIN_PASS" \
    --email    "$ADMIN_EMAIL" \
    --admin \
    --must-change-password=false 2>&1 | grep -v "already exists" || true
echo "✓ Admin user ready"

# ── 6. Create admin token via REST API ──────────────────────────────────────

echo "🔑 Creating admin token '$ADMIN_TOKEN_NAME' ..."
curl -sf -X DELETE \
    -u "$ADMIN_USER:$ADMIN_PASS" \
    "$FORGEJO_URL/api/v1/users/$ADMIN_USER/tokens/$ADMIN_TOKEN_NAME" \
    > /dev/null 2>&1 || true

ADMIN_TOKEN=$(curl -sf -X POST \
    -u "$ADMIN_USER:$ADMIN_PASS" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$ADMIN_TOKEN_NAME\",\"scopes\":[\"write:admin\",\"write:repository\",\"write:issue\",\"write:user\",\"read:admin\",\"read:repository\",\"read:issue\",\"read:user\"]}" \
    "$FORGEJO_URL/api/v1/users/$ADMIN_USER/tokens" \
    | grep -o '"sha1":"[^"]*"' | cut -d'"' -f4)

if [[ -z "$ADMIN_TOKEN" ]]; then
    echo "❌ Failed to create admin token via API."
    exit 1
fi
echo "✓ Admin token obtained"

# ── 7. Terraform: fresh state ───────────────────────────────────────────────

cd "$ROOT/infra/test"
rm -rf .terraform terraform.tfstate terraform.tfstate.backup
terraform init -input=false -upgrade >/dev/null

echo "🏗  Applying Terraform ..."
TF_VAR_forgejo_admin_token="$ADMIN_TOKEN" \
TF_VAR_forgejo_admin_password="$ADMIN_PASS" \
TF_VAR_sidecar_password="$SIDECAR_PASS" \
TF_VAR_dispatcher_password="$DISPATCHER_PASS" \
TF_VAR_reviewer_password="$REVIEWER_PASS" \
TF_VAR_human_password="$HUMAN_PASS" \
terraform apply -auto-approve -input=false -var-file=test.tfvars
echo "✓ Terraform apply complete"

# ── 8. Set passwords + create sidecar token ─────────────────────────────────

cd "$ROOT"

echo "👷 Setting worker passwords ..."
WORKER_LOGINS=$(cd "$ROOT/infra/test" && terraform output -json worker_passwords 2>/dev/null | grep -o '"[^"]*":' | tr -d '":' || true)
for login in $WORKER_LOGINS; do
    curl -sf -X PATCH \
        -H "Authorization: token $ADMIN_TOKEN" \
        -H "Content-Type: application/json" \
        -d "{\"login_name\":\"$login\",\"source_id\":0,\"password\":\"$WORKER_PASS\",\"must_change_password\":false}" \
        "$FORGEJO_URL/api/v1/admin/users/$login" > /dev/null 2>&1 || true
done
echo "✓ Worker passwords set"

echo "🔑 Creating sidecar API token ..."
curl -sf -X PATCH \
    -H "Authorization: token $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"login_name\":\"$SIDECAR_LOGIN\",\"source_id\":0,\"password\":\"$SIDECAR_PASS\",\"must_change_password\":false}" \
    "$FORGEJO_URL/api/v1/admin/users/$SIDECAR_LOGIN" > /dev/null

# Delete existing token if present, then create fresh.
curl -sf -X DELETE \
    -u "$SIDECAR_LOGIN:$SIDECAR_PASS" \
    "$FORGEJO_URL/api/v1/users/$SIDECAR_LOGIN/tokens/$SIDECAR_TOKEN_NAME" \
    > /dev/null 2>&1 || true

_token_resp=$(curl -sf -X POST \
    -u "$SIDECAR_LOGIN:$SIDECAR_PASS" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$SIDECAR_TOKEN_NAME\",\"scopes\":[\"write:issue\",\"read:issue\",\"write:repository\",\"read:repository\",\"read:user\",\"write:user\"]}" \
    "$FORGEJO_URL/api/v1/users/$SIDECAR_LOGIN/tokens")
SIDECAR_TOKEN=$(echo "$_token_resp" | grep -o '"sha1":"[^"]*"' | cut -d'"' -f4)

if [[ -z "$SIDECAR_TOKEN" ]]; then
    echo "❌ Failed to create sidecar API token. Response: $_token_resp"
    exit 1
fi
echo "✓ Sidecar token created"

echo "🔑 Creating dispatcher API token ..."
curl -sf -X PATCH \
    -H "Authorization: token $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"login_name\":\"$DISPATCHER_LOGIN\",\"source_id\":0,\"password\":\"$DISPATCHER_PASS\",\"must_change_password\":false}" \
    "$FORGEJO_URL/api/v1/admin/users/$DISPATCHER_LOGIN" > /dev/null

# Delete existing token if present, then create fresh.
curl -sf -X DELETE \
    -u "$DISPATCHER_LOGIN:$DISPATCHER_PASS" \
    "$FORGEJO_URL/api/v1/users/$DISPATCHER_LOGIN/tokens/$DISPATCHER_TOKEN_NAME" \
    > /dev/null 2>&1 || true

_disp_token_resp=$(curl -sf -X POST \
    -u "$DISPATCHER_LOGIN:$DISPATCHER_PASS" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$DISPATCHER_TOKEN_NAME\",\"scopes\":[\"write:issue\",\"read:issue\",\"write:repository\",\"read:repository\"]}" \
    "$FORGEJO_URL/api/v1/users/$DISPATCHER_LOGIN/tokens")
DISPATCHER_TOKEN=$(echo "$_disp_token_resp" | grep -o '"sha1":"[^"]*"' | cut -d'"' -f4)

if [[ -z "$DISPATCHER_TOKEN" ]]; then
    echo "❌ Failed to create dispatcher API token. Response: $_disp_token_resp"
    exit 1
fi
echo "✓ Dispatcher token created"

echo "🔑 Creating reviewer API token ..."
curl -sf -X PATCH \
    -H "Authorization: token $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"login_name\":\"$REVIEWER_LOGIN\",\"source_id\":0,\"password\":\"$REVIEWER_PASS\",\"must_change_password\":false}" \
    "$FORGEJO_URL/api/v1/admin/users/$REVIEWER_LOGIN" > /dev/null

# Delete existing token if present, then create fresh.
curl -sf -X DELETE \
    -u "$REVIEWER_LOGIN:$REVIEWER_PASS" \
    "$FORGEJO_URL/api/v1/users/$REVIEWER_LOGIN/tokens/$REVIEWER_TOKEN_NAME" \
    > /dev/null 2>&1 || true

_rev_token_resp=$(curl -sf -X POST \
    -u "$REVIEWER_LOGIN:$REVIEWER_PASS" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$REVIEWER_TOKEN_NAME\",\"scopes\":[\"write:issue\",\"read:issue\",\"write:repository\",\"read:repository\"]}" \
    "$FORGEJO_URL/api/v1/users/$REVIEWER_LOGIN/tokens")
REVIEWER_TOKEN=$(echo "$_rev_token_resp" | grep -o '"sha1":"[^"]*"' | cut -d'"' -f4)

if [[ -z "$REVIEWER_TOKEN" ]]; then
    echo "❌ Failed to create reviewer API token. Response: $_rev_token_resp"
    exit 1
fi
echo "✓ Reviewer token created"

echo "👤 Setting human reviewer password ..."
curl -sf -X PATCH \
    -H "Authorization: token $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"login_name\":\"$HUMAN_LOGIN\",\"source_id\":0,\"password\":\"$HUMAN_PASS\",\"must_change_password\":false}" \
    "$FORGEJO_URL/api/v1/admin/users/$HUMAN_LOGIN" > /dev/null
echo "✓ Human reviewer '$HUMAN_LOGIN' password set"

# ── 9. Write .sidecar.env ───────────────────────────────────────────────────

cat > .sidecar.env <<EOF
FORGEJO_URL=$FORGEJO_URL
FORGEJO_TOKEN=$SIDECAR_TOKEN
DISPATCHER_FORGEJO_TOKEN=$DISPATCHER_TOKEN
REVIEWER_FORGEJO_TOKEN=$REVIEWER_TOKEN
REVIEWER_HUMAN_LOGIN=$HUMAN_LOGIN
NATS_URL=nats://localhost:4223
DB_PATH=$ROOT/.data/workflow.db
LISTEN_ADDR=0.0.0.0:8080
EOF
echo "✓ Wrote .sidecar.env"

# ── 10. Push workflow files to repo ────────────────────────────────────────

echo "📄 Pushing workflow files to repo ..."
REPO_OWNER="${REPO_OWNER:-sysadmin}"
REPO_NAME="${REPO_NAME:-workflow-test}"

for wf in action/sim-work.yml action/agent-work.yml; do
    fname=$(basename "$wf")
    target_path=".forgejo/workflows/$fname"
    wf_content=$(base64 < "$wf")

    # Check if file exists (get its sha for update)
    existing_sha=$(curl -sf \
        -H "Authorization: token $ADMIN_TOKEN" \
        "$FORGEJO_URL/api/v1/repos/$REPO_OWNER/$REPO_NAME/contents/$target_path" \
        2>/dev/null | grep -o '"sha":"[^"]*"' | cut -d'"' -f4 || true)

    if [[ -n "$existing_sha" ]]; then
        curl -sf -X PUT \
            -H "Authorization: token $ADMIN_TOKEN" \
            -H "Content-Type: application/json" \
            -d "{\"content\":\"$wf_content\",\"message\":\"update $fname\",\"sha\":\"$existing_sha\"}" \
            "$FORGEJO_URL/api/v1/repos/$REPO_OWNER/$REPO_NAME/contents/$target_path" \
            > /dev/null
    else
        curl -sf -X POST \
            -H "Authorization: token $ADMIN_TOKEN" \
            -H "Content-Type: application/json" \
            -d "{\"content\":\"$wf_content\",\"message\":\"add $fname workflow\"}" \
            "$FORGEJO_URL/api/v1/repos/$REPO_OWNER/$REPO_NAME/contents/$target_path" \
            > /dev/null
    fi
    echo "  ✓ $target_path"
done

# ── 11. Build ────────────────────────────────────────────────────────────────

echo "🔨 Building binaries ..."
cargo build -p workflow-sidecar -p workflow-cdc -p worker-cli 2>&1 | grep -v "^$" | tail -3
echo "✓ Build complete"

# ── 12. Start CDC + sidecar ─────────────────────────────────────────────────

echo "🚀 Starting CDC process ..."
FORGEJO_DB_PATH="$ROOT/.data/forgejo/gitea/gitea.db" \
NATS_URL=nats://localhost:4223 \
RUST_LOG=workflow_cdc=info \
    "$ROOT/target/debug/workflow-cdc" > /tmp/workflow-cdc.log 2>&1 &
CDC_PID=$!
echo "✓ CDC started (pid $CDC_PID)"

echo "🚀 Starting sidecar ..."
env $(cat .sidecar.env | xargs) \
RUST_LOG=workflow_sidecar=info \
    "$ROOT/target/debug/workflow-sidecar" > /tmp/workflow-sidecar.log 2>&1 &
SIDECAR_PID=$!
echo "✓ Sidecar started (pid $SIDECAR_PID)"

# Wait for sidecar to be ready
echo "⏳ Waiting for sidecar ..."
for i in $(seq 1 30); do
    curl -sf http://localhost:8080/jobs > /dev/null 2>&1 && break
    sleep 1
done
curl -sf http://localhost:8080/jobs > /dev/null 2>&1 || { echo "❌ Sidecar did not start"; exit 1; }
echo "✓ Sidecar is up"

# ── 13. Seed fixture ────────────────────────────────────────────────────────

if [[ "$SEED" == "true" ]]; then
    echo "🌱 Seeding fixture: $FIXTURE ..."
    env $(cat .sidecar.env | xargs) SIDECAR_URL=http://localhost:8080 \
        "$ROOT/target/debug/worker-cli" seed sysadmin/workflow-test "$FIXTURE"

    # Brief pause for CDC to catch up
    sleep 2

    echo ""
    echo "📊 Graph state:"
    curl -sf http://localhost:8080/jobs | python3 -c "
import sys,json
jobs = json.load(sys.stdin)['jobs']
for j in sorted(jobs, key=lambda x: x['number']):
    deps = j['dependency_numbers']
    print(f'  #{j[\"number\"]:2d} {j[\"state\"]:12s} deps={str(deps):16s} {j[\"title\"]}')
print(f'\n  Total: {len(jobs)} jobs')
"
fi

# ── Done ──────────────────────────────────────────────────────────────────

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Setup complete!"
echo ""
echo "  Graph viewer:  http://localhost:8080/graph"
echo "  Forgejo:       $FORGEJO_URL"
echo "  Sidecar API:   http://localhost:8080"
echo ""
echo "  Human reviewer: $HUMAN_LOGIN / $HUMAN_PASS"
echo "    Log in at $FORGEJO_URL to review PRs manually"
echo ""
echo "  Sidecar PID:   $SIDECAR_PID  (log: /tmp/workflow-sidecar.log)"
echo "  CDC PID:       $CDC_PID  (log: /tmp/workflow-cdc.log)"
echo ""
echo "  To stop:  pkill -f workflow-sidecar; pkill -f workflow-cdc"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
