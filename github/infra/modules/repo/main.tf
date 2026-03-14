terraform {
  required_providers {
    github = {
      source  = "integrations/github"
      version = "~> 6.0"
    }
  }
}

resource "github_repository" "this" {
  name        = var.name
  description = var.description
  visibility  = var.visibility

  auto_init              = true
  delete_branch_on_merge = true

  allow_squash_merge = var.allow_squash_merge
  allow_merge_commit = var.allow_merge_commit
  allow_rebase_merge = var.allow_rebase_merge

  security_and_analysis {
    secret_scanning {
      status = "enabled"
    }
    secret_scanning_push_protection {
      status = "enabled"
    }
  }
}

resource "github_branch_default" "this" {
  repository = github_repository.this.name
  branch     = var.default_branch
}

resource "github_branch_protection" "default" {
  count = var.require_pr_before_merging ? 1 : 0

  repository_id = github_repository.this.node_id
  pattern       = var.default_branch

  required_pull_request_reviews {
    required_approving_review_count = var.required_approving_review_count
    dismiss_stale_reviews           = var.dismiss_stale_reviews
  }

  required_status_checks {
    strict   = var.strict_status_checks
    contexts = var.required_status_check_contexts
  }

  require_signed_commits = var.require_signed_commits

  required_linear_history = var.require_linear_history
}
