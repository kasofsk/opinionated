#!/bin/sh
# Code duplication pre-commit hook: runs jscpd on changed directories
#
# Expects: STAGED_FILES environment variable (newline-separated list of staged paths)
# Uses .jscpd.json from repo root if present.

CHANGED_DIRS=$(echo "$STAGED_FILES" | xargs -I {} dirname {} | sort -u | grep -v '^\.$' | grep -v '^\.' | head -20)

if [ -z "$CHANGED_DIRS" ]; then
    exit 0
fi

echo "==> Duplication: running jscpd on changed directories..."
if ! echo "$CHANGED_DIRS" | xargs npx jscpd@4.0.8; then
    echo "Code duplication check failed. Run 'npx jscpd .' to see details."
    exit 1
fi
