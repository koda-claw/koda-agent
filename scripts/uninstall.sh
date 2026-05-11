#!/usr/bin/env bash
set -euo pipefail

PREFIX="${KODA_AGENT_PREFIX:-$HOME/.local}"
REMOVE_DATA=0
DRY_RUN=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --remove-data) REMOVE_DATA=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) echo 'Usage: scripts/uninstall.sh [--prefix DIR] [--remove-data] [--dry-run]'; exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done
run() { if [[ "$DRY_RUN" == 1 ]]; then printf '+ %q' "$@"; printf '\n'; else "$@"; fi; }
run rm -f "$PREFIX/bin/koda-agent"
if [[ "$REMOVE_DATA" == 1 ]]; then
  run rm -rf "${KODA_AGENT_HOME:-$HOME/.koda-agent}" "$HOME/.config/koda-agent"
fi
echo 'Uninstall complete.'
