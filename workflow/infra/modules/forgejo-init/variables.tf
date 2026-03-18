# ── Forgejo connection (needed for null_resource provisioners) ─────────────────

variable "forgejo_url" {
  description = "Base URL of the Forgejo instance (e.g. http://localhost:3000)."
  type        = string
}

variable "forgejo_admin_token" {
  description = "Admin API token used to create labels and webhooks via the REST API."
  type        = string
  sensitive   = true
}

# ── Repository ────────────────────────────────────────────────────────────────

variable "repo_owner" {
  description = "Forgejo username or org that will own the workflow repository."
  type        = string
}

variable "repo_name" {
  description = "Name of the workflow repository to create."
  type        = string
  default     = "workflow"
}

variable "repo_description" {
  description = "Repository description."
  type        = string
  default     = "Workflow task tracking"
}

variable "repo_private" {
  description = "Whether the repository is private."
  type        = bool
  default     = false
}

# ── Sidecar service account ───────────────────────────────────────────────────

variable "sidecar_login" {
  description = "Username for the sidecar service account."
  type        = string
  default     = "workflow-sidecar"
}

variable "sidecar_email" {
  description = "Email address for the sidecar service account."
  type        = string
}

variable "sidecar_password" {
  description = "Password for the sidecar service account. Auto-generated if empty."
  type        = string
  default     = ""
  sensitive   = true
}

# ── Workers ───────────────────────────────────────────────────────────────────

variable "workers" {
  description = <<-EOT
    Map of worker login → configuration.
    Example:
      workers = {
        "worker-alpha" = { email = "alpha@example.com" }
      }
  EOT
  type = map(object({
    email = string
  }))
  default = {}
}

variable "worker_default_password" {
  description = "Initial password for worker accounts. Auto-generated per worker if empty."
  type        = string
  default     = ""
  sensitive   = true
}

# ── Webhook ───────────────────────────────────────────────────────────────────

variable "sidecar_url" {
  description = <<-EOT
    URL Forgejo uses to POST webhooks to the sidecar.
    The module appends "/webhook" automatically.
  EOT
  type = string
}
