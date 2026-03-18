#!/usr/bin/env bash
# init.sh — sets up (or resets) the workflow dev/test environment.
#
# Always destroys any existing Terraform-managed resources and recreates
# them from scratch. Never patches or modifies existing state.
#
# What it does:
#   1. Starts Forgejo + NATS via docker compose
#   2. Waits for Forgejo to be healthy
#   3. Creates the admin user via CLI (idempotent)
#   4. Creates an admin API token via REST API (delete-then-create)
#   5. Destroys any existing Terraform state, then applies fresh
#   6. Sets the sidecar account password via admin API
#   7. Creates the sidecar API token via REST API
#   8. Writes .sidecar.env so you can start the sidecar locally
#
# Usage:
#   ./scripts/init.sh
#   source .sidecar.env && cargo run -p workflow-sidecar
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

FORGEJO_URL="${FORGEJO_URL:-http://localhost:3000}"
ADMIN_USER="${ADMIN_USER:-sysadmin}"
ADMIN_PASS="${ADMIN_PASS:-admin1234}"
ADMIN_EMAIL="${ADMIN_EMAIL:-sysadmin@test.local}"
ADMIN_TOKEN_NAME="${ADMIN_TOKEN_NAME:-terraform}"

SIDECAR_LOGIN="${SIDECAR_LOGIN:-workflow-sidecar}"
SIDECAR_PASS="${SIDECAR_PASS:-sidecar-test-1234}"
SIDECAR_TOKEN_NAME="workflow-sidecar"

# ── 1. Start infra ────────────────────────────────────────────────────────────

echo "🐳 Starting infra (forgejo + nats) ..."
cd "$ROOT"
docker compose up -d forgejo nats
echo "✓ Containers started"

# ── 2. Wait for Forgejo ───────────────────────────────────────────────────────

echo "⏳ Waiting for Forgejo at $FORGEJO_URL ..."
until curl -sf "$FORGEJO_URL/api/v1/version" > /dev/null 2>&1; do
    sleep 2
done
echo "✓ Forgejo is up"

# ── 3. Create admin user (idempotent) ─────────────────────────────────────────

echo "👤 Creating admin user '$ADMIN_USER' ..."
docker compose exec -T -u git forgejo \
    gitea admin user create \
    --username "$ADMIN_USER" \
    --password "$ADMIN_PASS" \
    --email    "$ADMIN_EMAIL" \
    --admin \
    --must-change-password=false 2>&1 | grep -v "already exists" || true
echo "✓ Admin user ready"

# ── 4. Create admin token via REST API ────────────────────────────────────────
# Delete any existing token with this name, then create fresh.

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

# ── 5. Terraform: destroy then apply ─────────────────────────────────────────
# Always wipe existing resources first so we start from a clean slate.

cd "$ROOT/infra/test"
terraform init -input=false -upgrade >/dev/null

echo "🗑  Destroying existing Terraform resources ..."
TF_VAR_forgejo_admin_token="$ADMIN_TOKEN" \
TF_VAR_forgejo_admin_password="$ADMIN_PASS" \
TF_VAR_sidecar_password="$SIDECAR_PASS" \
terraform destroy -auto-approve -input=false -var-file=test.tfvars \
    2>&1 | grep -E "(Destroying|Destruction complete|No objects need|destroy complete)" || true
echo "✓ Destroy complete"

echo "🏗  Applying Terraform ..."
TF_VAR_forgejo_admin_token="$ADMIN_TOKEN" \
TF_VAR_forgejo_admin_password="$ADMIN_PASS" \
TF_VAR_sidecar_password="$SIDECAR_PASS" \
terraform apply -auto-approve -input=false -var-file=test.tfvars
echo "✓ Terraform apply complete"

# ── 6. Set sidecar password and create API token ──────────────────────────────
# The Forgejo tokens API requires basic auth (user's own credentials).
# Set the password explicitly via admin API to guarantee it matches SIDECAR_PASS.

echo "🔑 Creating sidecar API token ..."

curl -sf -X PATCH \
    -H "Authorization: token $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"login_name\":\"$SIDECAR_LOGIN\",\"source_id\":0,\"password\":\"$SIDECAR_PASS\",\"must_change_password\":false}" \
    "$FORGEJO_URL/api/v1/admin/users/$SIDECAR_LOGIN" > /dev/null

_token_resp=$(curl -sf -X POST \
    -u "$SIDECAR_LOGIN:$SIDECAR_PASS" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$SIDECAR_TOKEN_NAME\",\"scopes\":[\"write:issue\",\"read:issue\",\"write:repository\",\"read:repository\",\"read:user\"]}" \
    "$FORGEJO_URL/api/v1/users/$SIDECAR_LOGIN/tokens")
SIDECAR_TOKEN=$(echo "$_token_resp" | grep -o '"sha1":"[^"]*"' | cut -d'"' -f4)

if [[ -z "$SIDECAR_TOKEN" ]]; then
    echo "❌ Failed to create sidecar API token. Response: $_token_resp"
    exit 1
fi

echo "✓ Sidecar token created"

# ── 7. Write .sidecar.env ─────────────────────────────────────────────────────

cd "$ROOT"
mkdir -p .data

cat > .sidecar.env <<EOF
FORGEJO_URL=$FORGEJO_URL
FORGEJO_TOKEN=$SIDECAR_TOKEN
NATS_URL=nats://localhost:4223
DB_PATH=$ROOT/.data/workflow.db
LISTEN_ADDR=0.0.0.0:8080
EOF

echo "✓ Wrote .sidecar.env"

# ── Done ──────────────────────────────────────────────────────────────────────

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  Setup complete! Start the sidecar:"
echo ""
echo "    source .sidecar.env && cargo run -p workflow-sidecar"
echo ""
echo "  Then export test env vars and run tests:"
echo ""
echo "    source <(cd infra/test && terraform output -raw env_exports)"
echo "    export FORGEJO_TOKEN=\$(grep FORGEJO_TOKEN .sidecar.env | cut -d= -f2)"
echo ""
echo "    cargo test -p workflow-integration-tests -- --include-ignored"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
