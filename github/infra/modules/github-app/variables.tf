variable "app_slug" {
  description = "The slug (URL-friendly name) of the GitHub App."
  type        = string
}

variable "installation_id" {
  description = "The installation ID of the GitHub App on the org. Found in the App's installation URL or via the API."
  type        = string
}

variable "permitted_repos" {
  description = "List of repository names the App installation is scoped to. The App will have zero access to any repo not listed here."
  type        = list(string)
}

variable "permissions" {
  description = <<-EOT
    Permissions the App should be registered with in GitHub. This is
    documentation-only — actual permissions are set when registering the App.
    Declared here so Terraform plans surface permission intent alongside repo scope.

    Admin permissions (administration, organization_administration) are
    blocked by validation — agents must never hold admin access.

    Example:
      {
        contents      = "write"
        pull_requests = "write"
        issues        = "read"
        metadata      = "read"
      }
  EOT
  type        = map(string)
  default = {
    contents      = "write"
    pull_requests = "write"
    issues        = "read"
    metadata      = "read"
  }

  validation {
    condition     = !contains(keys(var.permissions), "administration")
    error_message = "Admin permissions are not allowed for agent tokens. Remove 'administration' from permissions."
  }

  validation {
    condition     = !contains(keys(var.permissions), "organization_administration")
    error_message = "Org admin permissions are not allowed for agent tokens. Remove 'organization_administration' from permissions."
  }
}
