# Browser Extension Bridge

`frontend tmwebdriver` starts the Rust TMWebDriver-compatible master. Installed
users should load the unpacked extension copy under Koda home:

```text
~/.koda-agent/browser/tmwd_cdp_bridge/
```

Source checkouts keep pristine bridge assets under:

```text
assets/tmwd_cdp_bridge/
```

Runtime config is generated in the mutable home copy:

```text
~/.koda-agent/browser/tmwd_cdp_bridge/config.js
```

This file is ignored and must not be committed. Reload the unpacked extension in
Edge or Chrome after config or asset changes. If the home copy is missing, run:

```bash
koda-agent resources install --repair
koda-agent doctor
```

## Smoke tests

Start the master in one terminal:

```bash
make tmwebdriver
```

Then run in another terminal:

```bash
make smoke-tmwd-extension
make smoke-tmwd-matrix
```

`contentSettings` mutation is skipped by default. Set
`KODA_TMWD_SMOKE_MUTATE=1` when you want to run the allow-and-restore safety
check.

## Common issues

- `Cannot load JavaScript config.js`: run `koda-agent doctor` after
  `koda-agent resources install --repair`, then reload the unpacked extension
  from `~/.koda-agent/browser/tmwd_cdp_bridge`.
- `Cannot read properties of null`: reload the extension and make sure the page
  tab is fully loaded before executing DOM-dependent commands.
- No extension sessions: confirm the master is running and the extension has
  access to `127.0.0.1:18765` and `127.0.0.1:18766`.
