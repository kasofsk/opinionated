variable "org_name" {
  description = "The GitHub organization name."
  type        = string
}

variable "default_branch" {
  description = "The default branch name to protect."
  type        = string
  default     = "main"
}

variable "github_apps" {
  description = <<-EOT
    GitHub Apps to install on the org with scoped repo access.
    Each entry represents one actor (human or agent) that needs
    gh CLI access with verifiable, limited permissions.

    Example:
      {
        ai-contributor = {
          app_slug        = "my-ai-contributor-app"
          installation_id = "12345678"
          permitted_repos = ["frontend", "docs"]
          permissions     = {
            contents      = "write"
            pull_requests = "write"
            issues        = "read"
            metadata      = "read"
          }
        }
        owner = {
          app_slug        = "my-owner-app"
          installation_id = "87654321"
          permitted_repos = ["frontend", "backend", "infra"]
          permissions     = {
            contents       = "write"
            pull_requests  = "write"
            issues         = "write"
            administration = "read"
            metadata       = "read"
          }
        }
      }
  EOT
  type = map(object({
    app_slug        = string
    installation_id = string
    permitted_repos = list(string)
    permissions     = optional(map(string), {
      contents      = "write"
      pull_requests = "write"
      issues        = "read"
      metadata      = "read"
    })
  }))
  default = {}
}
