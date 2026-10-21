# Sourced by hooks: re-exec the calling hook inside the flake dev shell
# when cargo/jq are not on PATH (Claude Code started outside direnv).
# The hook payload survives the re-exec through HOOK_INPUT.
HOOK_INPUT=${HOOK_INPUT:-$(cat)}
export HOOK_INPUT
if [ -z "${CALLREC_HOOK_IN_DEVSHELL:-}" ] && ! { command -v cargo && command -v jq; } >/dev/null 2>&1; then
  export CALLREC_HOOK_IN_DEVSHELL=1
  exec nix develop --option warn-dirty false "$CLAUDE_PROJECT_DIR" --command "$0" "$@"
fi
