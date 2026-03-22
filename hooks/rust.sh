#!/bin/sh
# Rust pre-commit hook: format, re-stage, and lint
#
# Expects: STAGED_FILES environment variable (newline-separated list of staged paths)
# Configurable env vars:
#   RUST_PATHS  — grep pattern for Rust-relevant paths (default: Cargo workspace root files + common crate dirs)

RUST_PATHS="${RUST_PATHS:-^(Cargo\.(toml|lock))}"

RUST_CHANGED=$(echo "$STAGED_FILES" | grep -E "$RUST_PATHS")

if [ -z "$RUST_CHANGED" ]; then
    exit 0
fi

echo "==> Rust: running cargo fmt..."
cargo fmt --all
# Re-stage any formatted files
echo "$RUST_CHANGED" | while IFS= read -r file; do
    [ -f "$file" ] && git add "$file"
done

echo "==> Rust: running cargo clippy..."
if ! cargo clippy --workspace --all-targets --all-features -- -D warnings; then
    echo "cargo clippy failed. Fix warnings/errors before committing."
    exit 1
fi
