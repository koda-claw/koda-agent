# koda-agent

Rust implementation of GenericAgent-compatible core behavior, aligned to upstream `lsdefine/GenericAgent` commit `9024af7`.


## Install

Source install from this checkout:

```bash
scripts/install.sh --from-source
```

Release install after a GitHub repository is configured:

```bash
curl -fsSL https://raw.githubusercontent.com/<owner>/<repo>/main/scripts/install.sh \
  | KODA_AGENT_REPO=<owner>/<repo> sh
```

Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/install.ps1 -FromSource
```

Python helpers are optional. Use `koda-agent bootstrap-python --extras core --repair` only when reflect scripts, OCR, vision helpers, or upstream Python SOPs need Python. See `docs/installation.md`, `docs/configuration.md`, `docs/security.md`, and `docs/browser-extension.md` for install, config, public-repo, and browser bridge guidance.

Installed runtime data lives under `~/.koda-agent` by default. The current
directory remains the workspace for file tools. Packaged prompts, tool schemas,
memory SOPs, browser bridge assets, and Python requirement files are copied into
`~/.koda-agent/resources` by the installer or via:

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
```

Configuration is read from `.env` (`OPENAI_BASE_URL`, `OPENAI_API_KEY`, `OPENAI_MODEL`) and optional `config/llms.toml`. If TOML is absent, simple upstream-style `mykey.json` or `mykey.py` dict assignments are imported as a compatibility fallback. Set `OPENAI_API_STYLE=responses` to use the OpenAI Responses API wire shape, or `OPENAI_API_STYLE=claude` for Anthropic `/v1/messages`. Secrets are redacted in logs.

## Validation

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## Configuration

Priority is `.env` first, then `config/llms.toml`, then optional legacy `mykey.json` / `mykey.py` compatibility:

```toml
[default]
base_url = "https://api.openai.com/v1"
api_key = "sk-..."
model = "gpt-4.1-mini"
api_style = "chat" # or "responses"
max_turns = 70
stream = false
timeout_secs = 600 # set 0 to disable total request timeout for very long responses
temperature = 1.0
max_tokens = 8192
reasoning_effort = "medium"
service_tier = "auto"
proxy = ""
failover = true

[mixin]
llm_nos = [0, "backup"] # optional MixinSession-style ordered fallback group
max_retries = 3
base_delay = 1.5
spring_back = 300

[[models]]
name = "backup"
base_url = "https://api.openai.com/v1"
api_key = "sk-..."
model = "gpt-4.1"
api_style = "chat"
stream = true

[models.headers]
"x-provider-header" = "value"
```

Use `/llms` to list configured models and `/llm <n>` to switch. Runtime requests use a GenericAgent-like MixinSession policy: ordered fallback over `mixin.llm_nos`, retry rounds with exponential backoff, and spring-back to the primary model after `spring_back` seconds. Transient timeout/429/5xx/connect failures fail over; 400/protocol errors do not.

`--task <IODIR>` follows the upstream file-I/O mode: the first process detaches and writes `temp/<IODIR>/stdout.log` / `stderr.log`; the worker consumes `input.txt`, writes intermediate/final `output*.txt`, restores `_history.json` when present, honors `_stop`, and waits up to 10 minutes for `reply.txt` rounds. Use hidden `--nobg` only when you intentionally want foreground task execution for debugging.

`tui` supports multi-session commands (`/sessions`, `/new`, `/branch`, `/switch`, `/rewind`) plus runtime slash commands. The stable default is the line-mode TUI. The experimental full-screen Ratatui cockpit is available with `tui --full`; set `KODA_TUI_FULL=1` to try it through the plain `tui` command, and use `tui --line` to force the stable fallback. Full-screen shortcuts include `Enter` submit, `Ctrl-J` newline, `Ctrl-S` stop, `Ctrl-N` new session, `Ctrl-B` branch, `Ctrl-W` close, `Ctrl-L` clear timeline, `Ctrl-P` command palette, `?` help, `PageUp`/`PageDown` or mouse wheel timeline scroll, `End` return to latest output, `Esc` close overlay/quit, and `Ctrl-Q` immediate quit. Timeline and Inspector both render scrollbars; Timeline clamps to real content height, auto-follows new output until the user scrolls away, shows unseen output hints, and renders a lightweight Markdown subset with Chinese-first role labels; mouse click focuses a pane, and mouse wheel/trackpad scroll targets the pane under the cursor when possible. macOS terminals usually intercept `Command-*` before terminal apps can receive it, so full-screen TUI uses terminal-portable `Ctrl-*` shortcuts and F-key alternatives: `F1` help, `F2` command palette, `F3` new session, `F4` branch, `F5` clear, `F6` close. Local full-screen commands include `/branch [name]`, `/switch <id|name>`, `/rename <name>`, `/sessions`, `/clear`, `/close`, `/help`, and `/commands`; runtime commands such as `/status`, `/llm <n>`, `/llms`, `/continue`, and `/btw <question>` pass through to the agent. `serve-acp` exposes an ACP JSON-RPC-over-JSONL bridge compatible with GenericAgent's `frontends/genericagent_acp_bridge.py`; the legacy `{"prompt":"..."}` / `{"input":"..."}` JSONL fallback remains available for simple local smoke tests.

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
