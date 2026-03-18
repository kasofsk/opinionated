variable "forgejo_url" {
  type = string
}

variable "forgejo_admin_user" {
  description = "Admin username for basic auth (required by provider to create repos and tokens)."
  type        = string
}

variable "forgejo_admin_password" {
  description = "Admin password for basic auth."
  type        = string
  sensitive   = true
}

variable "forgejo_admin_token" {
  description = "Admin API token used by null_resource provisioners (labels, webhooks)."
  type        = string
  sensitive   = true
}

variable "forgejo_insecure" {
  type    = bool
  default = false
}

variable "repo_owner" {
  type = string
}

variable "repo_name" {
  type    = string
  default = "workflow-test"
}

variable "sidecar_login" {
  type    = string
  default = "workflow-sidecar"
}

variable "worker_logins" {
  type    = list(string)
  default = ["test-worker"]
}

variable "email_domain" {
  type    = string
  default = "test.local"
}

variable "sidecar_url" {
  description = "URL Forgejo uses to POST webhooks to the sidecar."
  type        = string
}

variable "sidecar_api_url" {
  description = "URL the test process uses to call the sidecar API."
  type        = string
  default     = "http://localhost:8080"
}

variable "sidecar_password" {
  description = "Password for the sidecar service account. Passed through to the module."
  type        = string
  default     = ""
  sensitive   = true
}
