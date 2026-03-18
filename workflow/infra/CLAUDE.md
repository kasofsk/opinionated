# CLAUDE.md — infra

Terraform configurations for provisioning Forgejo resources.

## Structure

- `modules/forgejo-init/` — Reusable module: creates users, repos, labels, collaborator permissions
- `test/` — Test environment config, references the module with test-specific variables (`test.tfvars`)
- `dev/` — Dev environment config

## Provider

Uses **Lerentis/gitea** Terraform provider. This provider requires username+password auth (not token-only) for the admin user.

## Labels are created via `curl`

The gitea provider has no label resource. Labels (status labels like `on-deck`, `blocked`, `rework`, `revoked`, etc.) are provisioned via `null_resource` + `local-exec` curl commands. If you change label names, also update `JobState` string conversions in `crates/types/src/lib.rs`.

## Forgejo app.ini

`forgejo/app.ini` is the base Forgejo configuration. It's copied into `.data/forgejo/gitea/conf/` by `init.sh` before first boot. Includes CORS configuration so the graph viewer (served at `:8080`) can make cross-origin API calls to Forgejo (`:3000`).

## Two sidecar service accounts

The sidecar uses two Forgejo identities for audit trail clarity:
- **`workflow-sync`** (variable: `sidecar_login`) — label/dep management
- **`workflow-dispatcher`** (variable: `dispatcher_login`) — assignee/comment management

Both are provisioned by the `forgejo-init` module and added as repo collaborators.

## No webhooks

Webhooks were removed in favor of the CDC pipeline. The sidecar learns about Forgejo changes via the CDC process reading Forgejo's SQLite DB directly, not via webhook callbacks. The CDC also detects merged PRs via SQLite queries (`pull_request` table with `has_merged=1` and body matching `Closes #N`).

## Worker permissions

Workers are created as Forgejo users with `write` collaborator access on the repo. This gives them permission to push branches, create PRs, and comment on issues, but NOT admin/merge access. Three workers are provisioned by default: `worker-aria`, `worker-blake`, `worker-casey`.

## Running Terraform

Always use `scripts/init.sh` rather than running Terraform directly — the script handles admin user/token creation, fresh Terraform state, worker password setup, sidecar token provisioning, and `.sidecar.env` generation.

## Variables requiring secrets

- `forgejo_admin_token` — passed via `TF_VAR_forgejo_admin_token` env var (created by init.sh)
- `forgejo_admin_password` — passed via `TF_VAR_forgejo_admin_password`
- `sidecar_password` — passed via `TF_VAR_sidecar_password`
- `dispatcher_password` — passed via `TF_VAR_dispatcher_password`
