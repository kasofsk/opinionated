terraform {
  required_providers {
    github = {
      source  = "integrations/github"
      version = "~> 6.0"
    }
  }
}

data "github_organization" "org" {
  name = var.org_name
}

resource "github_organization_ruleset" "no_force_push_default" {
  name        = "no-force-push-default-branch"
  target      = "branch"
  enforcement = "active"

  conditions {
    ref_name {
      include = ["~DEFAULT_BRANCH"]
      exclude = []
    }
  }

  rules {
    non_fast_forward = true
  }
}

# --- GitHub Apps for scoped agent/user access ---

module "github_app" {
  source   = "../github-app"
  for_each = var.github_apps

  app_slug        = each.value.app_slug
  installation_id = each.value.installation_id
  permitted_repos = each.value.permitted_repos
  permissions     = each.value.permissions
}
