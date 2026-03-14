output "app_id" {
  description = "The numeric ID of the GitHub App (used to generate installation tokens)."
  value       = data.github_app.this.id
}

output "app_node_id" {
  description = "The GraphQL node ID of the GitHub App."
  value       = data.github_app.this.node_id
}

output "app_slug" {
  description = "The slug of the GitHub App."
  value       = var.app_slug
}

output "installation_id" {
  description = "The installation ID (used to generate installation tokens)."
  value       = var.installation_id
}

output "permitted_repos" {
  description = "Repos this App installation can access."
  value       = var.permitted_repos
}

output "declared_permissions" {
  description = "The permissions this App is expected to have (documentation-only, set at App registration)."
  value       = var.permissions
}
