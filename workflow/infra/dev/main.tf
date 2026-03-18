terraform {
  required_version = ">= 1.5"

  required_providers {
    gitea = {
      source  = "Lerentis/gitea"
      version = "~> 0.16"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }
}

provider "gitea" {
  base_url = var.forgejo_url
  token    = var.forgejo_admin_token
  # Set insecure = true when using a self-signed cert in local dev.
  insecure = var.forgejo_insecure
}

module "forgejo" {
  source = "../modules/forgejo-init"

  # Repo
  repo_owner       = var.repo_owner
  repo_name        = var.repo_name
  repo_description = "Workflow task tracking (dev)"
  repo_private     = false

  # Sidecar service account
  sidecar_login = "workflow-sidecar"
  sidecar_email = "sidecar@${var.email_domain}"

  # Workers — add as many as you need
  workers = {
    "worker-alpha" = { email = "worker-alpha@${var.email_domain}" }
    "worker-beta"  = { email = "worker-beta@${var.email_domain}" }
  }

  # URL Forgejo uses to reach the sidecar (docker service name in compose)
  sidecar_url = var.sidecar_url
}

# ── Outputs ───────────────────────────────────────────────────────────────────

output "repo" {
  value = module.forgejo.repo_html_url
}

output "sidecar_token" {
  value     = module.forgejo.sidecar_token
  sensitive = true
}

output "worker_passwords" {
  value     = module.forgejo.worker_initial_passwords
  sensitive = true
}
