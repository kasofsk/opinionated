# ── Passwords ─────────────────────────────────────────────────────────────────

resource "random_password" "sidecar" {
  length  = 32
  special = true
}

# Always generate one per worker; locals decide whether to use it.
resource "random_password" "workers" {
  for_each = var.workers
  length   = 24
  special  = true
}

locals {
  sidecar_password = var.sidecar_password != "" ? var.sidecar_password : random_password.sidecar.result

  worker_passwords = {
    for login, _ in var.workers :
    login => var.worker_default_password != "" ? var.worker_default_password : random_password.workers[login].result
  }
}

# ── Users ─────────────────────────────────────────────────────────────────────

resource "gitea_user" "sidecar" {
  username             = var.sidecar_login
  login_name           = var.sidecar_login
  password             = local.sidecar_password
  email                = var.sidecar_email
  must_change_password = false
  admin                = false
}

resource "gitea_user" "workers" {
  for_each = var.workers

  username             = each.key
  login_name           = each.key
  password             = local.worker_passwords[each.key]
  email                = each.value.email
  must_change_password = true
  admin                = false
}

# ── Repository ────────────────────────────────────────────────────────────────

resource "gitea_repository" "workflow" {
  username          = var.repo_owner
  name              = var.repo_name
  description       = var.repo_description
  private           = var.repo_private
  auto_init         = true
  has_issues        = true
  has_pull_requests = true
  has_wiki          = false
  has_projects      = false
}

# ── Status labels ─────────────────────────────────────────────────────────────
#
# The Lerentis/gitea provider has no gitea_label resource, so we use the
# Forgejo REST API directly via null_resource + local-exec.
# Labels are created idempotently (curl ignores 422 Unprocessable).

locals {
  status_labels = {
    "status:on-deck" = {
      color       = "#0075ca"
      description = "Ready to be claimed by a worker"
    }
    "status:blocked" = {
      color       = "#e4e669"
      description = "Waiting on one or more dependencies"
    }
    "status:on-the-stack" = {
      color       = "#fbca04"
      description = "Claimed — a worker is actively executing this"
    }
    "status:in-review" = {
      color       = "#8b5cf6"
      description = "Work complete, awaiting human review"
    }
    "status:done" = {
      color       = "#0e8a16"
      description = "Reviewed and merged"
    }
    "status:failed" = {
      color       = "#b60205"
      description = "Worker reported failure"
    }
    "status:on-ice" = {
      color       = "#cfd3d7"
      description = "Deliberately paused — will not be claimed"
    }
  }
}

resource "null_resource" "labels" {
  for_each = local.status_labels

  triggers = {
    repo  = "${var.repo_owner}/${gitea_repository.workflow.name}"
    name  = each.key
    color = each.value.color
  }

  provisioner "local-exec" {
    command = <<-CMD
      curl -sf -X POST \
        -H "Authorization: token ${var.forgejo_admin_token}" \
        -H "Content-Type: application/json" \
        -d '{"name":"${each.key}","color":"${each.value.color}","description":"${each.value.description}"}' \
        "${var.forgejo_url}/api/v1/repos/${var.repo_owner}/${gitea_repository.workflow.name}/labels" \
        > /dev/null || true
    CMD
  }

  depends_on = [gitea_repository.workflow]
}

# ── Sidecar webhook ───────────────────────────────────────────────────────────

resource "null_resource" "webhook" {
  triggers = {
    repo = "${var.repo_owner}/${gitea_repository.workflow.name}"
    url  = "${trimsuffix(var.sidecar_url, "/")}/webhook"
  }

  provisioner "local-exec" {
    command = <<-CMD
      curl -sf -X POST \
        -H "Authorization: token ${var.forgejo_admin_token}" \
        -H "Content-Type: application/json" \
        -d '{"type":"gitea","config":{"url":"${trimsuffix(var.sidecar_url, "/")}/webhook","content_type":"json"},"events":["issues"],"active":true}' \
        "${var.forgejo_url}/api/v1/repos/${var.repo_owner}/${gitea_repository.workflow.name}/hooks" \
        > /dev/null || true
    CMD
  }

  depends_on = [gitea_repository.workflow]
}

