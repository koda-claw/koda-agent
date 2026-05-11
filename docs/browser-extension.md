# Browser Extension Bridge

`frontend tmwebdriver` starts the Rust TMWebDriver-compatible master for the
unpacked extension in `assets/tmwd_cdp_bridge/`.

Local runtime config is generated as:

```text
assets/tmwd_cdp_bridge/config.js
```

This file is ignored and must not be committed. Reload the unpacked extension in
Edge or Chrome after config or asset changes.

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

- `Cannot load JavaScript config.js`: run any command that initializes the
  runtime directories, for example `koda-agent doctor`, then reload the unpacked
  extension.
- `Cannot read properties of null`: reload the extension and make sure the page
  tab is fully loaded before executing DOM-dependent commands.
- No extension sessions: confirm the master is running and the extension has
  access to `127.0.0.1:18765` and `127.0.0.1:18766`.
