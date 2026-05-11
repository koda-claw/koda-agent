#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

printf '%s\n' '== tracked suspicious path scan =='
git ls-files | rg -i '(^\.env$|^config/llms\.toml$|(^|/)\.koda-agent/|memory/L4_raw_sessions/(session_.*\.json|all_histories\.txt)$|(^|/)logs/|\.bak$|erp_order_query_sop\.md|assets/tmwd_cdp_bridge/config\.js$|browser/tmwd_cdp_bridge/config\.js$)' || true

printf '%s\n' '== ignored private/runtime files =='
git status --ignored --short | rg -i '(\.env|\.koda-agent|L4_raw_sessions|logs|erp_order|config\.js|\.bak)' || true

printf '%s\n' '== tracked content scan =='
if git grep -n -i \
  -e 'OPENAI_API_KEY[[:space:]]*=' \
  -e 'sk-[A-Za-z0-9_-]\{12,\}' \
  -e '__RequestVerificationToken' \
  -e 'CQW_ERPManagerSite_Identity' \
  -e 'erp\.ssbooking' \
  -e 'ssbooking' \
  -e 'password[[:space:]]*[:=]' \
  -e 'passwd[[:space:]]*[:=]' \
  -e 'secret[[:space:]]*[:=]' \
  -- ':!docs.GenericAgent.README.md' ':!memory/L4_raw_sessions/**' ':!scripts/audit-secrets.sh' ':!scripts/audit-history.sh'; then
  printf '\n%s\n' 'Review every match above. Placeholder/test matches may be acceptable; real secrets are not.'
else
  printf '%s\n' 'No tracked content matches found.'
fi
