#!/usr/bin/env bash
set -euo pipefail
if command -v koda-agent >/dev/null 2>&1; then
  exec koda-agent doctor --json
fi
if [[ -x ./target/debug/koda-agent ]]; then
  exec ./target/debug/koda-agent doctor --json
fi
exec cargo run -q -p koda-agent-cli -- doctor --json
