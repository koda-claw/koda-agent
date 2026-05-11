# Security and Public Repository Checklist

Do not commit local runtime data or secrets.

Ignored private/runtime paths include:

- `.env`
- `logs/`
- `crates/*/logs/`
- `memory/global_mem.txt`
- `memory/global_mem_insight.txt`
- `memory/file_access_stats.json`
- `memory/long_term_updates.jsonl`
- `memory/L4_raw_sessions/all_histories.txt`
- `memory/L4_raw_sessions/session_*.json`
- `memory/erp_order_query_sop.md`
- `assets/tmwd_cdp_bridge/config.js`
- `*.bak`

Before pushing to a public remote, run:

```bash
make audit-secrets
make audit-history
make check
```

The audit commands intentionally print placeholder/test matches such as
`.env.example`, `sk-test`, and redaction tests. Review every match and reject any
real API key, cookie, password, ERP token, browser session, or private SOP.

If runtime files were ever committed, rewrite history before pushing. For a new
repository with no remote, the safest path is to back up the current repository
with `git bundle create`, then create a clean orphan commit from the current
working tree.
