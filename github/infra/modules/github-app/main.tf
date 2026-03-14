terraform {
  required_providers {
    github = {
      source  = "integrations/github"
      version = "~> 6.0"
    }
  }
}

# Fetch the App to validate it exists and surface its metadata.
data "github_app" "this" {
  slug = var.app_slug
}

# Scope the installation to exactly the listed repos — nothing else.
# This is the core security boundary: the App physically cannot access
# repos not listed here, regardless of what permissions it holds.
resource "github_app_installation_repositories" "this" {
  installation_id    = var.installation_id
  selected_repositories = var.permitted_repos
}
