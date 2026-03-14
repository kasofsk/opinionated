output "repository_name" {
  description = "The repository name."
  value       = github_repository.this.name
}

output "repository_full_name" {
  description = "The full repository name (org/repo)."
  value       = github_repository.this.full_name
}

output "repository_html_url" {
  description = "The URL of the repository on GitHub."
  value       = github_repository.this.html_url
}
