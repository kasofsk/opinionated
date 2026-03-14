#!/bin/bash
# check-gh-token.sh - Block gh CLI unless using an explicitly allowed token type
#
# Claude Code pre-tool-use hook for the Bash tool.
# Default-deny: blocks ALL gh commands unless the caller explicitly opts in
# to a token type via env vars.
#
# Configuration (env vars):
#   GH_HOOK_ALLOW_PAT=1   - Allow fine-grained PATs (github_pat_*)
#   GH_HOOK_ALLOW_APP=1   - Allow GitHub App installation tokens (ghs_*)
#   GH_ALLOWED_TOKEN_HASH  - SHA-256 hash of the approved token (required).
#                            Generate with: gh auth token | tr -d '\n' | shasum -a 256
#
# Optional additional checks:
#   GH_ALLOWED_USER        - Required GitHub username or app slug. If set, the
#                            authenticated identity must match.
#
# Admin permissions are blocked at the token creation level — GitHub Apps
# should be registered without admin scopes, and fine-grained PATs should
# be created without admin permissions. See github/infra/modules/github-app/.
#
# Setup: add to .claude/settings.json under hooks.pre_tool_use:
#   {
#     "hooks": {
#       "pre_tool_use": [
#         {
#           "matcher": "Bash",
#           "hook": ".claude/hooks/check-gh-token.sh"
#         }
#       ]
#     }
#   }

set -euo pipefail

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# Only check gh commands
[[ ! "$COMMAND" =~ ^gh[[:space:]] ]] && exit 0

# ---------------------------------------------------------------------------
# 1. Require authentication
# ---------------------------------------------------------------------------
AUTH_STATUS=$(gh auth status --active 2>&1) || {
  echo "Blocked: gh is not authenticated. Run 'gh auth login' first." >&2
  exit 2
}

# ---------------------------------------------------------------------------
# 2. Require explicit opt-in to at least one token type
# ---------------------------------------------------------------------------
ALLOW_PAT="${GH_HOOK_ALLOW_PAT:-}"
ALLOW_APP="${GH_HOOK_ALLOW_APP:-}"

if [[ -z "$ALLOW_PAT" && -z "$ALLOW_APP" ]]; then
  echo "Blocked: no token type allowed. Set GH_HOOK_ALLOW_PAT=1 and/or GH_HOOK_ALLOW_APP=1." >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# 3. Identify token type and check against allowed types
# ---------------------------------------------------------------------------
ACTIVE_TOKEN=$(gh auth token 2>/dev/null)

if [[ "$ACTIVE_TOKEN" == github_pat_* ]]; then
  TOKEN_TYPE="pat"
  if [[ "$ALLOW_PAT" != "1" ]]; then
    echo "Blocked: fine-grained PAT detected but GH_HOOK_ALLOW_PAT is not set." >&2
    exit 2
  fi
elif [[ "$ACTIVE_TOKEN" == ghs_* ]]; then
  TOKEN_TYPE="app"
  if [[ "$ALLOW_APP" != "1" ]]; then
    echo "Blocked: GitHub App token detected but GH_HOOK_ALLOW_APP is not set." >&2
    exit 2
  fi
else
  echo "Blocked: token must be a fine-grained PAT (github_pat_*) or GitHub App installation token (ghs_*). Classic and OAuth tokens are never allowed." >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# 4. Pin to a specific token by hash (required)
#    Generate the hash:
#      gh auth token | tr -d '\n' | shasum -a 256
# ---------------------------------------------------------------------------
if [[ -z "${GH_ALLOWED_TOKEN_HASH:-}" ]]; then
  echo "Blocked: GH_ALLOWED_TOKEN_HASH is not set. You must pin to a specific token." >&2
  exit 2
fi

ACTIVE_HASH=$(echo -n "$ACTIVE_TOKEN" | shasum -a 256 | awk '{print $1}')
if [[ "$ACTIVE_HASH" != "$GH_ALLOWED_TOKEN_HASH" ]]; then
  echo "Blocked: active gh token does not match the approved token (hash mismatch)." >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# 5. Verify authenticated identity (optional)
#    For PATs: checks the GitHub username.
#    For App tokens: checks the app slug (e.g. "my-ai-bot").
# ---------------------------------------------------------------------------
if [[ -n "${GH_ALLOWED_USER:-}" ]]; then
  if [[ "$TOKEN_TYPE" == "pat" ]]; then
    IDENTITY=$(gh api user --jq '.login' 2>/dev/null) || {
      echo "Blocked: could not verify authenticated GitHub user." >&2
      exit 2
    }
  else
    IDENTITY=$(gh api /app --jq '.slug' 2>/dev/null) || {
      echo "Blocked: could not verify GitHub App identity." >&2
      exit 2
    }
  fi
  if [[ "$IDENTITY" != "$GH_ALLOWED_USER" ]]; then
    echo "Blocked: authenticated as '$IDENTITY', expected '$GH_ALLOWED_USER'." >&2
    exit 2
  fi
fi

exit 0
