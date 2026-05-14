# koda-agent

Rust implementation of GenericAgent-compatible core behavior, aligned to upstream `lsdefine/GenericAgent` commit `9024af7`.

[Chinese README](README.zh.md)


## Install

Release install from GitHub Releases. The installer detects the current
platform, downloads the matching archive, verifies `SHA256SUMS` when available,
installs the binary, copies packaged resources into `~/.koda-agent/resources`,
and runs `koda-agent init`.

macOS / Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.sh \
  | KODA_AGENT_REPO=koda-claw/koda-agent sh
```

Install a specific release tag:

```bash
curl -fsSL https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.sh \
  | KODA_AGENT_REPO=koda-claw/koda-agent KODA_AGENT_VERSION=v0.1.7 sh
```

Windows PowerShell:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=$env:TEMP+'\koda-agent-install.ps1'; iwr -UseBasicParsing https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.ps1 -OutFile $s; & $s -Repo koda-claw/koda-agent"
```

Windows PowerShell with a specific release tag:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=$env:TEMP+'\koda-agent-install.ps1'; iwr -UseBasicParsing https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.ps1 -OutFile $s; & $s -Repo koda-claw/koda-agent -Version v0.1.7"
```

Source install from this checkout, useful for contributors:

```bash
scripts/install.sh --from-source
```

Windows source install from this checkout:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/install.ps1 -FromSource
```

After installation, open a new terminal if needed, then verify:

```bash
koda-agent --version
koda-agent doctor
```

Later updates can be done directly from GitHub Releases without keeping a
checkout:

```bash
koda-agent update --repo koda-claw/koda-agent --version latest
koda-agent update --repo koda-claw/koda-agent --version v0.1.7
koda-agent update --check
koda-agent update --check --json
```

The updater selects the current platform asset and currently supports
Linux/macOS/Windows on amd64 and arm64. It verifies `SHA256SUMS`, replaces the
installed binary, and repairs `~/.koda-agent/resources` by default.
`koda-agent --version` prints the installed CLI version.

Python helpers are optional. Use `koda-agent bootstrap-python --extras core --repair` only when reflect scripts, OCR, vision helpers, or upstream Python SOPs need Python. See `docs/installation.md`, `docs/configuration.md`, `docs/security.md`, and `docs/browser-extension.md` for install, config, public-repo, and browser bridge guidance.

## Chinese Documentation

- Chinese README: `README.zh.md`
- Quick start: `docs/book/src/quickstart.zh.md`
- CLI manual: `docs/book/src/cli.zh.md`
- LLM configuration: `docs/book/src/configuration.zh.md`
- TUI guide: `docs/book/src/tui.zh.md`
- Resources and Memory: `docs/book/src/resources-memory.zh.md`
- Release checklist: `docs/book/src/release-checklist.zh.md`

The same files can be rendered as a local tutorial site with mdBook:

```bash
make docs-serve
```

Installed runtime data lives under `~/.koda-agent` by default. The current
directory remains the workspace for file tools. Packaged prompts, tool schemas,
memory SOPs, browser bridge assets, and Python requirement files are copied into
`~/.koda-agent/resources` by `koda-agent init`, the installer, or via:

```bash
koda-agent resources install --repair
koda-agent resources doctor --json
```

## Quick start

```bash
cargo run -p koda-agent-cli -- --input "Say hello without tools"
cargo run -p koda-agent-cli -- --task demo --input "Read README.md" # prints background PID
cargo run -p koda-agent-cli -- --reflect ./watch.py
cargo run -p koda-agent-cli -- tui
cargo run -p koda-agent-cli -- tui --full # experimental full-screen TUI
cargo run -p koda-agent-cli -- frontend tmwebdriver
cargo run -p koda-agent-cli -- memory settle
cargo run -p koda-agent-cli -- memory settle --assisted
cargo run -p koda-agent-cli -- memory l4-archive --run
cargo run -p koda-agent-cli -- config setup mimo
cargo run -p koda-agent-cli -- config secret MIMO_API_KEY --from-stdin
cargo run -p koda-agent-cli -- update --dry-run
cargo run -p koda-agent-cli -- update --check --json
```

LLM configuration is `llms.toml` first. A profile now represents a provider/account/endpoint, and each profile owns one or more model aliases under `[[profiles.models]]`. Normal users can run `koda-agent config setup mimo`, save the key with `koda-agent config secret MIMO_API_KEY --from-stdin`, then launch `koda-agent tui --full`. Profiles live in `~/.koda-agent/config/llms.toml`; secrets plus `KODA_LLM_PROFILE` / `KODA_LLM_MODEL` live in `~/.koda-agent/.env`. Lookup checks the current directory, explicit workspace, `~/.koda-agent`, installed resources, and the platform config directory such as `~/.config/koda-agent`. Legacy `OPENAI_BASE_URL` / `OPENAI_MODEL` variables are migration inputs, not the primary runtime config. Secrets are redacted in logs.

## Validation

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## Contributing

See [CONTRIBUTING.md](.github/CONTRIBUTING.md) for development workflow, pre-push checklist, and CI guidelines.

## Configuration

The primary configuration is `~/.koda-agent/config/llms.toml` plus `~/.koda-agent/.env`:

```bash
koda-agent init
koda-agent config setup mimo --yes
koda-agent config secret MIMO_API_KEY --from-stdin
koda-agent config list
koda-agent config use mimo
koda-agent config model list mimo
koda-agent config setup deepseek --yes
koda-agent config model use deepseek flash
koda-agent config setup glm --yes
koda-agent config model use glm default
koda-agent config migrate # one-time import for legacy OPENAI_* .env users
koda-agent config validate
koda-agent doctor
```

```toml
[selector]
default_profile = "mimo"
default_model = "pro"

[defaults]
stream = true
timeout_secs = 1200
max_tokens = 16384
failover = true

[[profiles]]
name = "mimo"
kind = "native_oai"
base_url = "https://api.xiaomimimo.com/v1"
api_key_env = "MIMO_API_KEY"
auth_scheme = "header"
auth_header = "api-key"
api_mode = "chat_completions"

[[profiles.models]]
name = "pro"
id = "mimo-v2.5-pro"

[mixin]
llm_nos = ["mimo:pro", "backup:default"] # optional ordered fallback group
max_retries = 3
base_delay = 1.5
spring_back = 300

[[profiles]]
name = "backup"
kind = "native_oai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
api_mode = "responses"
stream = true

[profiles.headers]
"x-provider-header" = "value"

[[profiles.models]]
name = "default"
id = "gpt-5.2"
```

Use `/llms` to list configured models, `/llm <n|profile|profile:model>` to switch, and `/model <alias>` to switch within the active profile. Runtime requests use a GenericAgent-like MixinSession policy: ordered fallback over `mixin.llm_nos`, retry rounds with exponential backoff, and spring-back to the primary model after `spring_back` seconds. Transient timeout/429/5xx/connect failures fail over; 400/protocol errors do not.

`--task <IODIR>` follows the upstream file-I/O mode: the first process detaches and writes `temp/<IODIR>/stdout.log` / `stderr.log`; the worker consumes `input.txt`, writes intermediate/final `output*.txt`, restores `_history.json` when present, honors `_stop`, and waits up to 10 minutes for `reply.txt` rounds. Use hidden `--nobg` only when you intentionally want foreground task execution for debugging.

`tui` supports multi-session commands (`/sessions`, `/new`, `/branch`, `/switch`, `/rewind`) plus runtime slash commands. The stable default is the line-mode TUI. The experimental full-screen Ratatui cockpit is available with `tui --full`; set `KODA_TUI_FULL=1` to try it through the plain `tui` command, and use `tui --line` to force the stable fallback. Full-screen shortcuts include `Enter` submit, `Ctrl-J` newline, `Ctrl-S` stop, `Ctrl-N` new session, `Ctrl-B` branch, `Ctrl-W` close, `Ctrl-L` clear timeline, `Ctrl-P` command palette, `?` help, `PageUp`/`PageDown` or mouse wheel timeline scroll, `End` return to latest output, `Esc` close overlay/quit, and `Ctrl-Q` immediate quit. Timeline and Inspector both render scrollbars; Timeline clamps to real content height, auto-follows new output until the user scrolls away, shows unseen output hints, and renders a lightweight Markdown subset with Chinese-first role labels; mouse click focuses a pane, and mouse wheel/trackpad scroll targets the pane under the cursor when possible. macOS terminals usually intercept `Command-*` before terminal apps can receive it, so full-screen TUI uses terminal-portable `Ctrl-*` shortcuts and F-key alternatives: `F1` help, `F2` command palette, `F3` new session, `F4` branch, `F5` clear, `F6` close. Local full-screen commands include `/branch [name]`, `/switch <id|name>`, `/rename <name>`, `/sessions`, `/clear`, `/close`, `/help`, and `/commands`; runtime commands such as `/status`, `/llm <n|profile:model>`, `/llms`, `/models`, `/model <alias>`, `/continue`, and `/btw <question>` pass through to the agent. `serve-acp` exposes an ACP JSON-RPC-over-JSONL bridge compatible with GenericAgent's `frontends/genericagent_acp_bridge.py`; the legacy `{"prompt":"..."}` / `{"input":"..."}` JSONL fallback remains available for simple local smoke tests.

`make smoke-tui` verifies the full-screen TUI entrypoints without needing an interactive terminal: `tui --full` and `KODA_TUI_FULL=1 tui` must fail with a clear non-TTY fallback message, while `tui --line --help` remains accepted as the stable fallback.

`frontend tmwebdriver` starts the Rust TMWebDriver-compatible master used by `assets/tmwd_cdp_bridge/` (`ws://127.0.0.1:18765`, `/link` + `/api/longpoll` + `/api/result` on `127.0.0.1:18766`). `web_execute_js` treats direct JSON strings such as `{"cmd":"tabs"}`, `{"cmd":"cookies"}`, `{"cmd":"cdp","method":"Runtime.evaluate","params":{}}`, `{"cmd":"batch","commands":[...]}`, `{"cmd":"management","method":"list"}`, and `{"cmd":"contentSettings","type":"automaticDownloads","setting":"allow"}` as bridge commands before falling back to plain page JavaScript. `management` and `contentSettings` require the unpacked Chrome extension because plain CDP cannot access Chrome extension APIs.

`make smoke-browser` verifies a real Chrome CDP endpoint on `127.0.0.1:9222` by listing tabs and running `Runtime.evaluate` against the active page. Start Chrome with `--remote-debugging-port=9222` first if the smoke reports that CDP is unreachable.

`make smoke-rich-monitor` opens a local `data:` page through the same CDP endpoint and runs `web_execute_js` against it, covering no-change, DOM-changed, transient text, JavaScript error, new-tab, and reload/navigation monitor paths without using network or `.env`.

`make smoke-tmwd-extension` verifies the installed Edge/Chrome `tmwd_cdp_bridge` through the Rust master. Run `make tmwebdriver` in another terminal first; the smoke waits for extension sessions, then checks `tabs`, `management.list`, self-disable protection, `cdp Runtime.evaluate`, and `cookies` with cookie values redacted. `contentSettings` is skipped by default because it mutates browser settings; set `KODA_TMWD_SMOKE_MUTATE=1` when you want to include the allow + restore-to-ask safety check. Upstream already has `management list|reload|disable|enable` and `contentSettings`; this Rust port adds a guard so disabling the bridge extension itself requires `confirmSelf=true`.

`make smoke-tmwd-matrix` runs a fuller real-page matrix through the installed extension and Rust master. It serves local fixture pages, navigates a live browser tab, validates DOM form action, same-origin iframe access, cross-origin iframe blocking/error shape, download request delivery, autofill/password-field candidate detection, target=_blank new-tab reporting when the browser allows script-origin popups, CDP `Runtime.evaluate`, batch `$N.path` references, cookie extraction, management metadata, and the self-disable guard. Set `KODA_TMWD_SMOKE_MUTATE=1` to include the same `contentSettings` allow + restore mutation in the matrix.

`--reflect ./watch.py` polls a Python script compatible with GenericAgent's reflect mode. The script should expose `check() -> str | None`, optional `INTERVAL`, optional `ONCE`, and optional `on_done(result)`. Set `KODA_PYTHON=/path/to/python` if Python is not on PATH.

For machines without Python, `--reflect ./watch.json` uses a native rule:

```json
{"task":"Run /status","interval":5,"once":true}
```

Native scheduled-task reflection is also available without Python. Use `--reflect scheduler` or a JSON rule with `{"kind":"scheduler"}`; it scans `sche_tasks/*.json`, writes reports under `sche_tasks/done/`, supports `repeat` (`once`, `daily`, `weekday`, `weekly`, `monthly`, `every_2h`/`every_30m`/`every_1d`), `schedule`, `max_delay_hours`, `enabled`, and `prompt`, and keeps the same 120s polling interval as upstream unless `sche_tasks/_scheduler.json` overrides it.

Native Goal Mode is available with `--reflect goal_mode` or `{"kind":"goal_mode"}`. It reads `GOAL_STATE` or `temp/goal_state.json`, expects `objective`, optional `status`, `start_time`, `budget_seconds`, `turns_used`, and `max_turns`, wakes every 3 seconds, keeps pushing until budget/turns are exhausted, then switches state to `wrapping_up` and marks `done_budget` after the final `on_done` pass.

Native autonomous and agent-team reflection are available with `--reflect autonomous` / `{"kind":"autonomous"}` and `--reflect agent_team_worker` / `{"kind":"agent_team_worker"}`. Autonomous mode wakes every 30 minutes with the upstream auto-agent prompt. Agent-team mode reads `reflect/agent_team_setting.json` or inline JSON `base_url` / `board_key`, polls `/posts?limit=10` with `X-API-Key`, stores state in `temp/agent_team_worker_state.json`, and re-prompts for 120 seconds after `on_done` to follow up BBS replies.

`memory settle` processes structured `memory/long_term_updates.jsonl` entries into L1/L2/L3 when they are safe and action-verified, archives the queue, and moves empty or secret-like entries to `memory/pending_long_term_updates.md` for manual review. Add `--assisted` to ask the configured LLM to convert unsupported note-shaped entries into minimal safe JSON patches before deferring them.
Structured L2/L3 updates automatically create short L1 navigation pointers when safe; long/detail-like pointers are rejected so `global_mem_insight.txt` stays index-like.
`memory audit`, `memory cleanup`, and `memory recall` harden the native memory loop: grouped/table-style L1 entries are treated as coverage for referenced L3 SOP/helper files, cleanup writes a timestamped `global_mem_insight.*.bak` before changing L1, and recall searches both `all_histories.txt` and recent JSON L4 sessions with cleaner excerpts.
`cargo run -q -p xtask -- memory-parity-smoke` checks that upstream memory resources are present and exercises audit/cleanup/recall without mutating memory.

`memory l4-archive` mirrors the upstream L4 cron: it scans `temp/model_responses/model_responses_*.txt`, skips logs modified in the last 2 hours, compresses raw logs, appends recovered `<history>` blocks to `memory/L4_raw_sessions/all_histories.txt`, and stores compressed sessions in monthly zip archives. It is dry-run by default; pass `--run` to execute.

OCR/Vision helpers follow upstream `memory/ocr_utils.py` and `memory/vision_api.template.py`. Python `code_run` snippets can import `ocr_utils` or `vision_api` directly because the runtime injects the workspace memory path. Configure multimodal calls with `VISION_BASE_URL`, `VISION_API_KEY`, `VISION_MODEL`, and optional `VISION_BACKEND`, `VISION_API_KEY_HEADER`, `VISION_TOKEN_PARAM`, `VISION_SYSTEM_PROMPT`; `cargo run -q -p xtask -- vision-smoke` verifies the real endpoint without printing secrets.
Python helper runtime planning lives in `docs/python-runtime-strategy.md`; `koda-agent doctor --json` reports Python availability and managed venv status through the shared cross-platform resolver, `koda-agent bootstrap-python [--extras ocr,automation] [--recreate|--repair] [--dry-run|--offline]` creates or repairs the managed helper venv, and `koda-agent python-env remove` removes only that managed venv.

Optional `watch_file` plus `trigger: "exists"` or `"nonempty"` only fires when the file condition is met. Triggered results are appended under `temp/reflect_logs/`.
