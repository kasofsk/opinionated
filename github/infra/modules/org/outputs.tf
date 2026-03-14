output "org_name" {
  description = "The GitHub organization name."
  value       = data.github_organization.org.orgname
}

output "ruleset_id" {
  description = "The ID of the branch protection ruleset."
  value       = github_organization_ruleset.no_force_push_default.id
}

output "github_apps" {
  description = "Installed GitHub Apps with their IDs and repo scopes."
  value = {
    for name, app in module.github_app : name => {
      app_id          = app.app_id
      installation_id = app.installation_id
      permitted_repos = app.permitted_repos
      permissions     = app.declared_permissions
    }
  }
}
