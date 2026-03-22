#!/bin/sh
# Install pre-commit hooks
#
# Usage:
#   ./hooks/install.sh                  # interactive mode
#   ./hooks/install.sh rust flutter     # non-interactive: enable specific hooks
#
# This script:
#   1. Copies hook scripts into .githooks/
#   2. Writes .githooks-enabled with selected hooks
#   3. Sets git core.hooksPath to .githooks/

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(git rev-parse --show-toplevel)"
HOOKS_TARGET="$REPO_ROOT/.githooks"
CONFIG="$REPO_ROOT/.githooks-enabled"

AVAILABLE_HOOKS="rust flutter terraform duplication"

usage() {
    echo "Usage: $0 [hook1 hook2 ...]"
    echo ""
    echo "Available hooks:"
    echo "  rust         cargo fmt + clippy"
    echo "  flutter      dart format + flutter analyze + custom_lint"
    echo "  terraform    terraform fmt"
    echo "  duplication  jscpd code duplication check"
    echo ""
    echo "Run without arguments for interactive mode."
}

validate_hook() {
    for available in $AVAILABLE_HOOKS; do
        if [ "$1" = "$available" ]; then
            return 0
        fi
    done
    return 1
}

# Collect selected hooks
SELECTED=""

if [ $# -gt 0 ]; then
    # Non-interactive mode
    if [ "$1" = "-h" ] || [ "$1" = "--help" ]; then
        usage
        exit 0
    fi
    for hook in "$@"; do
        if ! validate_hook "$hook"; then
            echo "Unknown hook: $hook"
            usage
            exit 1
        fi
        SELECTED="$SELECTED $hook"
    done
else
    # Interactive mode
    echo "Select pre-commit hooks to enable:"
    echo ""
    for hook in $AVAILABLE_HOOKS; do
        case "$hook" in
            rust)        desc="cargo fmt + clippy" ;;
            flutter)     desc="dart format + flutter analyze + custom_lint" ;;
            terraform)   desc="terraform fmt" ;;
            duplication) desc="jscpd code duplication check" ;;
        esac
        printf "  Enable %s (%s)? [y/N] " "$hook" "$desc"
        read -r answer
        case "$answer" in
            [yY]*) SELECTED="$SELECTED $hook" ;;
        esac
    done
fi

if [ -z "$SELECTED" ]; then
    echo "No hooks selected. Exiting."
    exit 0
fi

# Create .githooks directory
mkdir -p "$HOOKS_TARGET"

# Copy dispatcher and selected hook scripts
cp "$SCRIPT_DIR/pre-commit" "$HOOKS_TARGET/pre-commit"
chmod +x "$HOOKS_TARGET/pre-commit"

for hook in $SELECTED; do
    cp "$SCRIPT_DIR/$hook.sh" "$HOOKS_TARGET/$hook.sh"
done

# Copy jscpd config if duplication hook is selected and no config exists
for hook in $SELECTED; do
    if [ "$hook" = "duplication" ] && [ ! -f "$REPO_ROOT/.jscpd.json" ]; then
        cp "$SCRIPT_DIR/jscpd.json" "$REPO_ROOT/.jscpd.json"
        echo "Created .jscpd.json (customize ignore patterns for your project)"
    fi
done

# Write config
: > "$CONFIG"
for hook in $SELECTED; do
    echo "$hook" >> "$CONFIG"
done

# Configure git
git config core.hooksPath .githooks

echo ""
echo "Installed hooks:$(echo "$SELECTED" | tr ' ' ', ')"
echo "Config written to .githooks-enabled"
echo "Git hooks path set to .githooks/"
echo ""
echo "To customize paths, create .githooks-env with overrides:"
echo "  FLUTTER_APP_DIR=mobile/app"
echo "  RUST_PATHS='^(src/|crates/|Cargo\.(toml|lock))'"
echo "  TERRAFORM_DIR=infra"
