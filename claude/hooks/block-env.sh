#!/bin/bash
# block-env.sh - Block Claude from reading, writing, or modifying .env files
#
# Claude Code pre-tool-use hook that prevents any interaction with .env files.
# Default-deny: .env files are never accessible regardless of context.
#
# Matches files like: .env, .env.local, .env.production, app/.env, etc.
#
# Setup: add to .claude/settings.json under hooks.PreToolUse:
#   {
#     "hooks": {
#       "PreToolUse": [
#         {
#           "matcher": "Read|Edit|Write|Glob|Grep|Bash",
#           "hooks": [
#             {
#               "type": "command",
#               "command": "claude/hooks/block-env.sh"
#             }
#           ]
#         }
#       ]
#     }
#   }

set -euo pipefail

INPUT=$(cat)
TOOL_NAME=$(echo "$INPUT" | jq -r '.tool_name // empty')

# ---------------------------------------------------------------------------
# Helper: check if a string references a .env file
# Matches: .env, .env.local, .env.production, path/to/.env, etc.
# Does NOT match: .environment, .envrc, node_modules/.env-example
# ---------------------------------------------------------------------------
is_env_file() {
  local path="$1"
  # Match .env exactly or .env.* (e.g. .env.local, .env.production)
  [[ "$path" =~ (^|/)\.env($|\.) ]]
}

# ---------------------------------------------------------------------------
# Tool-specific extraction and checking
# ---------------------------------------------------------------------------
case "$TOOL_NAME" in
  Read|Edit|Write)
    FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty')
    if is_env_file "$FILE_PATH"; then
      echo "Blocked: access to .env files is not allowed." >&2
      exit 2
    fi
    ;;
  Glob)
    PATTERN=$(echo "$INPUT" | jq -r '.tool_input.pattern // empty')
    if is_env_file "$PATTERN"; then
      echo "Blocked: searching for .env files is not allowed." >&2
      exit 2
    fi
    ;;
  Grep)
    GREP_PATH=$(echo "$INPUT" | jq -r '.tool_input.path // empty')
    GREP_GLOB=$(echo "$INPUT" | jq -r '.tool_input.glob // empty')
    if is_env_file "$GREP_PATH" || is_env_file "$GREP_GLOB"; then
      echo "Blocked: searching in .env files is not allowed." >&2
      exit 2
    fi
    ;;
  Bash)
    COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')
    # Check if the bash command references .env files
    if [[ "$COMMAND" =~ (^|[[:space:]=\"\'])(\.env($|[.\"/\' ]))|([\"/])\.env($|[.\"/\' ]) ]]; then
      echo "Blocked: bash commands referencing .env files are not allowed." >&2
      exit 2
    fi
    ;;
esac

exit 0
