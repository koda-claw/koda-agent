#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

echo '== history suspicious path scan =='
git log --all --name-only --pretty=format: | sort -u | rg -i '(^\.env$|^config/llms\.toml$|(^|/)\.koda-agent/|memory/L4_raw_sessions/(session_.*\.json|all_histories\.txt)$|(^|/)logs/|\.bak$|erp_order_query_sop\.md|assets/tmwd_cdp_bridge/config\.js$|browser/tmwd_cdp_bridge/config\.js$)' || true

echo '== history content scan =='
found=0
while IFS= read -r commit; do
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
    "$commit" -- ':!docs.GenericAgent.README.md' ':!memory/L4_raw_sessions/**' ':!scripts/audit-secrets.sh' ':!scripts/audit-history.sh' >/tmp/koda-agent-history-scan.$$ 2>/dev/null; then
    found=1
    sed "s/^/$commit:/" /tmp/koda-agent-history-scan.$$
  fi
done < <(git rev-list --all)
rm -f /tmp/koda-agent-history-scan.$$
if [[ "$found" == 0 ]]; then
  echo 'No history content matches found.'
else
  echo 'Review every match above. Placeholder/test matches may be acceptable; real secrets are not.'
fi
