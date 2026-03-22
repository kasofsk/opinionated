#!/bin/sh
# Flutter pre-commit hook: format, re-stage, analyze, and custom lint
#
# Expects: STAGED_FILES environment variable (newline-separated list of staged paths)
# Configurable env vars:
#   FLUTTER_APP_DIR — path to the Flutter app directory (default: .)

FLUTTER_APP_DIR="${FLUTTER_APP_DIR:-.}"
FLUTTER_PATHS="${FLUTTER_PATHS:-^${FLUTTER_APP_DIR}/}"

FLUTTER_CHANGED=$(echo "$STAGED_FILES" | grep -E "$FLUTTER_PATHS")

if [ -z "$FLUTTER_CHANGED" ]; then
    exit 0
fi

echo "==> Flutter: running dart format..."
(cd "$FLUTTER_APP_DIR" && dart format .)
# Re-stage any formatted files
echo "$FLUTTER_CHANGED" | while IFS= read -r file; do
    [ -f "$file" ] && git add "$file"
done

echo "==> Flutter: running flutter pub get..."
if ! (cd "$FLUTTER_APP_DIR" && flutter pub get --suppress-analytics); then
    echo "flutter pub get failed."
    exit 1
fi

echo "==> Flutter: running flutter analyze..."
if ! (cd "$FLUTTER_APP_DIR" && flutter analyze); then
    echo "flutter analyze failed. Fix warnings/errors before committing."
    exit 1
fi

echo "==> Flutter: running custom lint..."
if ! (cd "$FLUTTER_APP_DIR" && dart run custom_lint); then
    echo "dart run custom_lint failed. Fix warnings/errors before committing."
    exit 1
fi
