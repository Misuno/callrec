#!/usr/bin/env bash
# PostToolUse (Edit|Write): rustfmt the edited .rs file, then run clippy.
# Compile errors block (exit 2, fed back to Claude); warnings are passed
# back as context only, so half-finished work is not blocked on dead_code.
set -uo pipefail
HOOK_INPUT=${HOOK_INPUT:-$(cat)}
# Cheap pre-filter: skip non-Rust edits before entering the dev shell
grep -q '\.rs"' <<<"$HOOK_INPUT" || exit 0
. "$(dirname "$0")/devshell.sh"

file=$(jq -r '.tool_input.file_path // .tool_response.filePath // empty' <<<"$HOOK_INPUT")
case "$file" in *.rs) ;; *) exit 0 ;; esac
cd "$CLAUDE_PROJECT_DIR" || exit 0
[ -f Cargo.toml ] || exit 0

rustfmt --edition 2024 "$file" 2>/dev/null

out=$(cargo clippy --all-targets --quiet --message-format=short --color=never 2>&1)
status=$?
if [ "$status" -ne 0 ]; then
  { echo "cargo clippy failed after editing $file:"; echo "$out" | tail -n 80; } >&2
  exit 2
fi

warnings=$(echo "$out" | grep -E '\.rs:[0-9]+:[0-9]+: warning' | head -n 40)
if [ -n "$warnings" ]; then
  jq -n --arg w "$warnings" '{hookSpecificOutput: {hookEventName: "PostToolUse",
    additionalContext: ("cargo clippy warnings (must be fixed before finishing):\n" + $w)}}'
fi
exit 0
