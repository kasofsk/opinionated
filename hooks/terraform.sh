#!/bin/sh
# Terraform pre-commit hook: format and re-stage
#
# Expects: STAGED_FILES environment variable (newline-separated list of staged paths)
# Configurable env vars:
#   TERRAFORM_DIR — path to terraform directory (default: infra)

TERRAFORM_DIR="${TERRAFORM_DIR:-infra}"
TERRAFORM_PATHS="${TERRAFORM_PATHS:-^${TERRAFORM_DIR}/}"

TERRAFORM_CHANGED=$(echo "$STAGED_FILES" | grep -E "$TERRAFORM_PATHS")

if [ -z "$TERRAFORM_CHANGED" ]; then
    exit 0
fi

echo "==> Terraform: running terraform fmt..."
terraform fmt -recursive "$TERRAFORM_DIR"
# Re-stage any formatted files
echo "$TERRAFORM_CHANGED" | while IFS= read -r file; do
    [ -f "$file" ] && git add "$file"
done
