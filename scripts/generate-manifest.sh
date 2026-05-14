#!/usr/bin/env bash
# Generate resources/resources-manifest.json for remote resource updates.
#
# Usage:
#   ./scripts/generate-manifest.sh <version> [changelog]
#   e.g. ./scripts/generate-manifest.sh 1.1.0 "Added subagent SOP"
#
# Prerequisites:
#   - Run from repo root
#   - Release archives already built in dist/ (CI) or pass --local to build from source
#
# The manifest is used by `koda-agent resources check-update` / `resources update`.

set -euo pipefail

VERSION="${1:?Usage: $0 <version> [changelog]}"
CHANGELOG="${2:-}"
REPO="koda-claw/koda-agent"
TAG="v${VERSION}"
PUBLISHED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
RESOURCES_DIR="resources"
MANIFEST="${RESOURCES_DIR}/resources-manifest.json"
ARCHIVE_NAME="resources.tar.gz"

# Determine binary version from cargo metadata
BINARY_VERSION=$(cargo metadata --format-version=1 --no-deps 2>/dev/null \
  | python3 -c "import sys,json; pkgs=[p for p in json.load(sys.stdin)['packages'] if p['name']=='koda-agent-cli']; print(pkgs[0]['version'] if pkgs else 'unknown')")

# Build resources-only archive (excluding runtime/user files)
echo "==> Packing ${ARCHIVE_NAME} from assets/, memory/, config/, requirements-*.txt ..."
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

mkdir -p "${TMPDIR}/resources"
cp -R assets "${TMPDIR}/resources/"
cp -R memory "${TMPDIR}/resources/"
cp -R config "${TMPDIR}/resources/"
cp requirements-python-*.txt "${TMPDIR}/resources/" 2>/dev/null || true

# Exclude user/runtime files (same as release.yml)
rm -f "${TMPDIR}/resources/assets/tmwd_cdp_bridge/config.js" 2>/dev/null || true
rm -f "${TMPDIR}/resources/config/llms.toml" 2>/dev/null || true
rm -f "${TMPDIR}/resources/memory/global_mem.txt" \
      "${TMPDIR}/resources/memory/global_mem_insight.txt" \
      "${TMPDIR}/resources/memory/file_access_stats.json" \
      "${TMPDIR}/resources/memory/long_term_updates.jsonl" \
      "${TMPDIR}/resources/memory/pending_long_term_updates.md" 2>/dev/null || true
find "${TMPDIR}/resources/memory/L4_raw_sessions" -type f ! -name 'compress_session.py' -delete 2>/dev/null || true

tar -C "${TMPDIR}" -czf "${ARCHIVE_NAME}" resources

SHA256=$(shasum -a 256 "${ARCHIVE_NAME}" | awk '{print $1}')
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${TAG}/${ARCHIVE_NAME}"

echo "==> Generating ${MANIFEST} ..."
mkdir -p "${RESOURCES_DIR}"

cat > "${MANIFEST}" <<EOF
{
  "version": "${VERSION}",
  "published_at": "${PUBLISHED_AT}",
  "min_binary_version": "${BINARY_VERSION}",
  "download_url": "${DOWNLOAD_URL}",
  "sha256": "${SHA256}",
  "changelog": "${CHANGELOG}"
}
EOF

echo "==> Done. Manifest written to ${MANIFEST}:"
cat "${MANIFEST}"
echo ""
echo "==> Archive: ${ARCHIVE_NAME} ($(du -h ${ARCHIVE_NAME} | cut -f1))"
echo "==> SHA256:  ${SHA256}"
echo ""
echo "Next steps:"
echo "  1. git tag -a ${TAG} -m 'Release ${TAG}'"
echo "  2. git push origin ${TAG}  (CI will build + publish release)"
echo "  3. Upload ${ARCHIVE_NAME} to the GitHub Release assets"
echo "  4. git add ${MANIFEST} && git commit -m 'Update resources-manifest.json ${VERSION}'"
echo "  5. git push origin main"
