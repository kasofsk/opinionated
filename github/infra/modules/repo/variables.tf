variable "name" {
  description = "The repository name."
  type        = string
}

variable "description" {
  description = "A description of the repository."
  type        = string
  default     = ""
}

variable "visibility" {
  description = "Repository visibility: public or private."
  type        = string
  default     = "private"
}

variable "default_branch" {
  description = "The default branch name."
  type        = string
  default     = "main"
}

variable "require_pr_before_merging" {
  description = "Require pull requests before merging to the default branch."
  type        = bool
  default     = false
}

variable "required_approving_review_count" {
  description = "Number of required approving reviews when require_pr_before_merging is true."
  type        = number
  default     = 1
}

variable "dismiss_stale_reviews" {
  description = "Dismiss approvals when new commits are pushed."
  type        = bool
  default     = true
}

variable "required_status_check_contexts" {
  description = "List of status check contexts that must pass before merging."
  type        = list(string)
  default     = []
}

variable "strict_status_checks" {
  description = "Require the branch to be up-to-date with the base branch before merging."
  type        = bool
  default     = true
}

variable "require_signed_commits" {
  description = "Require signed commits on the protected branch."
  type        = bool
  default     = false
}

variable "require_linear_history" {
  description = "Require linear history (no merge commits) on the protected branch."
  type        = bool
  default     = false
}

variable "allow_squash_merge" {
  description = "Allow squash merges on the repository."
  type        = bool
  default     = true
}

variable "allow_merge_commit" {
  description = "Allow merge commits on the repository."
  type        = bool
  default     = false
}

variable "allow_rebase_merge" {
  description = "Allow rebase merges on the repository."
  type        = bool
  default     = false
}
