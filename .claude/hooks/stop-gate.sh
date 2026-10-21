#!/usr/bin/env bash
# Stop: quality gate before Claude finishes a turn with uncommitted Rust changes.
# fmt --check, clippy with -D warnings, cargo test. Failure blocks (exit 2).
set -uo pipefail
. "$(dirname "$0")/devshell.sh"

# Already continuing because of this hook: let the turn end to avoid a loop.
[ "$(jq -r '.stop_hook_active // false' <<<"$HOOK_INPUT")" = "true" ] && exit 0
cd "$CLAUDE_PROJECT_DIR" || exit 0
[ -f Cargo.toml ] || exit 0
[ -n "$(git status --porcelain -- '*.rs' Cargo.toml Cargo.lock clippy.toml 2>/dev/null)" ] || exit 0

fail() { { echo "$1"; echo "$2" | tail -n 80; } >&2; exit 2; }

out=$(cargo fmt --check 2>&1) || fail "cargo fmt --check failed (run cargo fmt):" "$out"
out=$(cargo clippy --all-targets --quiet --message-format=short --color=never -- -D warnings 2>&1) \
  || fail "cargo clippy -D warnings failed:" "$out"
out=$(cargo test --quiet --color=never 2>&1) || fail "cargo test failed:" "$out"
exit 0
