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
