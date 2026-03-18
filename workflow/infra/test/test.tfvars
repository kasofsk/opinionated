# Non-secret test environment defaults — safe to commit.
# Secrets (passwords, tokens) go in test.secrets.tfvars or TF_VAR_* env vars.

forgejo_url      = "http://localhost:3000"
forgejo_insecure = true

forgejo_admin_user = "sysadmin"   # must match ADMIN_USER in scripts/init.sh

repo_owner = "sysadmin"
repo_name  = "workflow-test"

sidecar_login = "workflow-sidecar"
worker_logins = ["test-worker"]

email_domain = "test.local"

# Fixed password for the sidecar service account so init.sh can use basic
# auth to create the API token (the Forgejo tokens endpoint requires it).
sidecar_password = "sidecar-test-1234"

# URL Forgejo (in Docker) uses to POST webhooks to the sidecar (on the host).
# On macOS/Windows Docker Desktop: host.docker.internal resolves to the host.
# On Linux: replace with your host IP.
sidecar_url = "http://host.docker.internal:8080"

sidecar_api_url = "http://localhost:8080"
