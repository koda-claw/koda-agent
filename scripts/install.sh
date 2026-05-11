#!/usr/bin/env bash
set -euo pipefail

REPO="${KODA_AGENT_REPO:-}"
VERSION="${KODA_AGENT_VERSION:-latest}"
PREFIX="${KODA_AGENT_PREFIX:-$HOME/.local}"
INSTALL_PYTHON="${KODA_AGENT_BOOTSTRAP_PYTHON:-0}"
DRY_RUN=0
FROM_SOURCE=0

usage() {
  cat <<USAGE
Usage: scripts/install.sh [options]

Options:
  --prefix DIR        Install prefix (default: ~/.local)
  --repo OWNER/REPO   GitHub repository for release downloads
  --version VERSION   Release tag, or latest (default: latest)
  --from-source       Build and install from the current checkout with cargo
  --bootstrap-python  Create/repair the managed helper Python environment
  --dry-run           Print actions without changing files
  -h, --help          Show this help

Environment:
  KODA_AGENT_REPO, KODA_AGENT_VERSION, KODA_AGENT_PREFIX,
  KODA_AGENT_BOOTSTRAP_PYTHON, KODA_AGENT_HOME
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --repo) REPO="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --from-source) FROM_SOURCE=1; shift ;;
    --bootstrap-python) INSTALL_PYTHON=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

run() {
  if [[ "$DRY_RUN" == 1 ]]; then
    printf '+ %q' "$@"; printf '\n'
  else
    "$@"
  fi
}

need() { command -v "$1" >/dev/null 2>&1 || { echo "Missing required command: $1" >&2; exit 1; }; }

BIN_DIR="$PREFIX/bin"
DATA_DIR="${KODA_AGENT_HOME:-$HOME/.koda-agent}"

copy_resources() {
  local src="$1"
  [[ -d "$src" ]] || return 0
  if [[ -x "$BIN_DIR/koda-agent" ]]; then
    run "$BIN_DIR/koda-agent" resources install --source "$src" --repair
  else
    run koda-agent resources install --source "$src" --repair
  fi
}

if [[ "$FROM_SOURCE" == 1 ]]; then
  need cargo
  run cargo install --path crates/koda-agent-cli --locked --root "$PREFIX" --force
  copy_resources "$(pwd)"
else
  need curl
  need tar
  if [[ -z "$REPO" ]]; then
    echo 'No release repository configured. Use --repo OWNER/REPO, set KODA_AGENT_REPO, or pass --from-source.' >&2
    exit 2
  fi
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)
  case "$os:$arch" in
    darwin:arm64) target=aarch64-apple-darwin ;;
    darwin:x86_64) target=x86_64-apple-darwin ;;
    linux:x86_64) target=x86_64-unknown-linux-gnu ;;
    linux:aarch64|linux:arm64) target=aarch64-unknown-linux-gnu ;;
    *) echo "Unsupported platform: $os/$arch" >&2; exit 1 ;;
  esac
  if [[ "$VERSION" == latest ]]; then
    url="https://github.com/$REPO/releases/latest/download/koda-agent-$target.tar.gz"
    sum_url="https://github.com/$REPO/releases/latest/download/SHA256SUMS"
  else
    url="https://github.com/$REPO/releases/download/$VERSION/koda-agent-$target.tar.gz"
    sum_url="https://github.com/$REPO/releases/download/$VERSION/SHA256SUMS"
  fi
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  run mkdir -p "$BIN_DIR" "$DATA_DIR"
  run curl -fsSL "$url" -o "$tmp/koda-agent.tar.gz"
  if curl -fsSL "$sum_url" -o "$tmp/SHA256SUMS" 2>/dev/null; then
    (cd "$tmp" && grep "koda-agent-$target.tar.gz" SHA256SUMS | shasum -a 256 -c -)
  else
    echo 'Warning: checksum file unavailable; skipping SHA256 verification.' >&2
  fi
  run tar -xzf "$tmp/koda-agent.tar.gz" -C "$tmp"
  run install -m 0755 "$tmp/koda-agent" "$BIN_DIR/koda-agent"
  copy_resources "$tmp/resources"
fi

run mkdir -p "$DATA_DIR" "$HOME/.config/koda-agent"

if [[ "$INSTALL_PYTHON" == 1 ]]; then
  if [[ -x "$BIN_DIR/koda-agent" ]]; then
    run "$BIN_DIR/koda-agent" bootstrap-python --extras core --repair
  else
    run koda-agent bootstrap-python --extras core --repair
  fi
fi

echo "Installed koda-agent under $PREFIX"
echo "Ensure $BIN_DIR is on PATH, then run: koda-agent doctor"
