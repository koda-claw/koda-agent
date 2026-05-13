# GenericAgent Upstream Parity Alignment

Baseline upstream: `/tmp/genericagent-inspect` at `lsdefine/GenericAgent` target `9024af7`.
Rust workspace: `/Users/vanzheng/projects/rust/koda-agent`.

This document is the stage gate before continuing feature work. It maps upstream files to Rust crates, records current parity, and fixes the next implementation order so later changes do not drift away from the original GenericAgent behavior.

## Parity Scale

- `P0`: behavior needed for core GenericAgent loop/tool/LLM compatibility.
- `P1`: important upstream behavior, needed before claiming broad parity.
- `P2`: platform/frontend breadth or polish; can be feature-gated.
- `Done`: implemented and covered by tests or smoke.
- `Partial`: implemented but missing edge behavior or upstream integrations.
- `Missing`: not yet implemented.

## Upstream Module Map

| Upstream | Rust target | Status | Notes |
|---|---|---:|---|
| `agentmain.py` | `crates/koda-agent-core`, `crates/koda-agent-cli` | Partial | Runtime, slash commands, task mode, logs, continue/new/resume are present. Background `--task`, `_history.json`, `_stop`, `reply.txt` are now closer. Still missing exact queue/thread display semantics and full CLI command parity. |
| `agent_loop.py` | `crates/koda-agent-core` | Done/P0 | `StepOutcome`, turn loop, tool dispatch, tool-result adjacency, stop handling, no-tool retry, plan intercept, runtime event stream implemented. |
| `ga.py` | `crates/koda-agent-tools`, `crates/koda-agent-core` | Partial | 9 atomic tools plus 2 memory tools implemented. Browser/file/code behavior is broad. Remaining gap is exact generator-yield streaming and some upstream browser monitor text details. |
| `llmcore.py` | `crates/koda-agent-llm`, `crates/koda-agent-core` | Done/P1 | Chat Completions, Responses, Claude Messages, DeepSeek reasoning replay, retries, per-model options, custom headers, proxy, Mixin-like failover, optional legacy `mykey.py`/`mykey.json` import, and profile-based LLM configuration (`config/llms.toml` with layered profile→model resolution) are implemented. Remaining gaps: exact provider-specific cache marker edge fixtures and more provider-specific option mappings. |
| `simphtml.py` | `crates/koda-agent-tools`, `assets/simphtml_*.js` | Partial | HTML simplification, list detection asset, smart truncation, DOM diff, rich JS monitor are implemented. Remaining gap: exact BeautifulSoup scoring/truncation parity and all rich monitor edge messages. |
| `TMWebDriver.py` + `assets/tmwd_cdp_bridge` | `crates/koda-agent-frontends`, `assets/tmwd_cdp_bridge` | Partial | WS + HTTP longpoll master, bridge commands, batch `$N.path`, extension config generation, CDP fallback implemented. Remaining gap: broader manual website edge cases beyond the automated local-page matrix, such as provider-specific autofill/password-manager prompts and download shelf UI details. |
| `frontends/genericagent_acp_bridge.py` | `crates/koda-agent-frontends` | Partial | ACP JSONL initialize/session/prompt/cancel/stop/shutdown and upstream-shaped session updates implemented; protocol fixture coverage added for init/errors/content blocks/active prompt/cancel. Need broader client interoperability smoke. |
| `frontends/tuiapp.py` | `crates/koda-agent-cli` | Partial/P2 | Multi-session line-mode TUI commands exist with a sidebar-like session panel, branch/switch/rename/rewind/fold/history/tail/view/search commands, and input history. Experimental `koda-agent tui --full` now provides a Ratatui full-screen layout with real per-session runtime creation, multiline composer input, async `AgentEvent` timeline updates, stop action, keyboard scrollback, branch/close/clear/rename/switch local session commands, help overlay, command palette, structured timeline markers, last-tool inspector detail, background unread/completion/failure notifications, ToggleMouseCapture (F7/Ctrl-M) with mode indicator, improved `ask_user` interaction flow, and TestBackend coverage. Missing heavyweight editor widget, memory/browser inspector panes, and final default-entry stability hardening. |
| `frontends/tgapp.py`, `fsapp.py`, `wecomapp.py`, `dingtalkapp.py` | `crates/koda-agent-frontends` | Partial/P2 | Telegram: streaming edit (MarkdownV2 + rate limit + code-fence line buffering), inline buttons (ask_user callback routing with menu_id, clear_markup on completion/timeout), file/image send (resolve_files + multipart upload + text fallback), 4 commands (/abort, /continue, /btw, /debug), proxy support (HTTPS_PROXY/HTTP_PROXY/ALL_PROXY), UserState stream_task handle management, TurnStreamCoordinator segment buffer with ≤50-line tail extraction — **65 unit tests**. Feishu/WeCom/DingTalk: webhook parsing/signature/reply scaffolds exist. Missing: Feishu/WeCom/DingTalk encrypted callbacks, upload/download. |
| `frontends/qqapp.py`, `wechatapp.py`, `qtapp.py`, `stapp*.py`, `desktop_pet*.pyw`, `hub.pyw`, `launch.pyw` | `crates/koda-agent-frontends` or future GUI crate | Missing/P2 | Only web/HTTP basic UI exists. Need platform feature gates before adding heavy GUI/IM dependencies. |
| `memory/*.md`, `memory/*.py` | `memory/`, `crates/koda-agent-memory` | Partial | SOP/resources migrated; ADB/OCR/keychain helpers, L4 compression, structured and assisted L1/L2/L3 settlement exist. Remaining gaps are stricter L1/L3 validation and autonomous memory hooks. |
| `reflect/*.py` | `crates/koda-agent-cli`, future scheduler | Partial | `--reflect` Python protocol, native JSON rules, native scheduler scan of `sche_tasks/*.json`, native goal/autonomous modes, native agent-team worker polling, and L4 cron are implemented. State writes are atomic, corrupt state is backed up/reset, and agent-team external-service failures now skip/log like upstream. Remaining gaps are richer fixture comparison and long-running external-service soak tests. |
| `plugins/langfuse_tracing.py` | `crates/koda-agent-core` | Partial/P2 | Opt-in JSONL trace mirror exists for LLM prompt/response and tool spans when `KODA_LANGFUSE_TRACE`, `config/langfuse.toml`, or `langfuse_config.json` is present. Remaining gap is direct Langfuse HTTP SDK/export integration. |
| `mykey_template*.py` | `.env`, `config/llms.toml`, optional `mykey.py`/`mykey.json` | Partial/P1 | Rust uses `.env` and TOML as primary configuration, with optional legacy import for simple upstream-style dict configs when TOML is absent. Secrets are not printed. |

## Phase Alignment

### Phase 0 - Workspace and Assets

Status: Done.

- Rust workspace, Makefile, `.env.example`, `rust-toolchain.toml`, config example exist.
- Core assets migrated: tool schemas, system prompts, memory templates, `simphtml` JS, CDP bridge static files.
- Runtime dirs are `temp/`, `memory/`, `logs/`.

Acceptance gate:

- `cargo fmt --all`
- `cargo test --workspace --all-features`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`

### Phase 1 - Core Agent Loop Parity

Status: Done/P0, with minor P1 refinements remaining.

Implemented:

- `AgentRuntime`, `StepOutcome`, turn history, tool dispatch, stop/abort, slash commands.
- Tool-call adjacency fixes for OpenAI/DeepSeek strict protocols.
- Working checkpoint injection, plan mode hooks, no-tool response retry, max-turn danger prompts.
- L4 raw session archival and model response logs.

Remaining:

- Queue serialization, running status, `/session.*` overrides, peer hint injection, and final-output cleanup are implemented; remaining display differences are mostly cosmetic.
- More exact restore/continue summarization prompt behavior.
- Generator-style incremental result semantics are approximated through `AgentEvent`; exact chunk thresholds are not byte-for-byte identical.

### Phase 2 - LLM Core Parity

Status: Partial/P0-P1.

Implemented:

- OpenAI-compatible Chat Completions.
- OpenAI Responses API.
- Claude `/v1/messages`.
- SSE parsers for all three styles.
- Native tool calls and internal tool-result conversion.
- DeepSeek `reasoning_content` replay.
- Retry/backoff, timeout tuning, per-model stream/timeout/options.
- MixinSession-like ordered fallback with retries, round delay, and spring-back.

Remaining priority:

1. Text-protocol `ToolClient` parity: prompt building, `<tool_use>` / JSON-ish textual tool-call parsing, `bad_json` fallback, and `last_tools` token-saving prompt reuse are implemented.
2. History trimming parity: `compress_history_tags()` and `_sanitize_leading_user_msg()` equivalents are implemented; remaining work is exact provider cache-marker behavior around trimmed histories.
3. Usage accounting parity: `_record_usage()` equivalent logs cache/token summaries to `logs/llm_usage.jsonl`.
4. Provider-specific cache markers are implemented for OAI-compatible Claude relays and Claude Messages user/system blocks; Claude signed-thinking replay now drops unsigned thinking and preserves signed blocks.
5. Optional `mykey.py`/`mykey.json` compatibility importer is implemented for simple upstream-style dict assignments; `.env`/TOML remains primary.

### Phase 3 - Tool Parity

Status: Partial/P0 mostly complete.

Implemented:

- `code_run`, `file_read`, `file_patch`, `file_write`, `web_scan`, `web_execute_js`, `ask_user`, `update_working_checkpoint`, `start_long_term_update`.
- File root-path safety, unicode-safe truncation, similar-file suggestions, memory access stats.
- Browser CDP discovery, JS execution, DOM diff, tab/list/cookie/batch bridge commands.
- Python is optional for Python code execution/reflection compatibility; core tools are Rust-native.

Remaining priority:

1. More `ga.py` golden fixtures for long-output truncation and code execution edge cases.
2. Rich browser monitor parity from `simphtml.execute_js_rich()`.
3. Full extension smoke for `management` / `contentSettings` / cookie edge cases.

### Phase 4 - Memory / Self-Evolution Parity

Status: Partial/P1.

Implemented:

- Memory directory bootstrap, L1/L2 files, SOP/resources migration.
- `update_working_checkpoint` runtime injection.
- `start_long_term_update` prompt entry and `long_term_updates.jsonl` queue.
- L4 session JSON + `all_histories.txt` archive.
- Native L4 compression/archive command `koda-agent memory l4-archive`: scans raw `model_responses_*.txt`, skips recent active logs, strips raw prompt/assistant echo, extracts `<history>`, appends `all_histories.txt`, and writes monthly zip archives. Reflect mode now runs the same archive pass on a 12h cadence like upstream scheduler.
- New long-task guard: after 15 turns, final answer is intercepted until long-term memory settlement is considered.
- Native structured settlement worker via `koda-agent memory settle`: applies safe structured L1/L2/L3 updates, archives the queue, and defers empty/secret-like entries to pending review. `--assisted` asks the configured LLM to convert unsupported note-shaped entries into minimal safe patches, closer to the upstream L0-guided memory update loop.
- Autonomous memory strategy is stricter: L2/L3 structured updates now auto-sync short L1 navigation pointers when no explicit pointer is provided, L1 pointers are normalized/capped and reject detail dumps, and the long-term update prompt prefers structured settlement over broad direct memory edits.
- L1/L4 maintenance is now safer than the first pass: memory audit recognizes upstream-style grouped L1 lines such as `L3: sop_a | helper.py`, excludes inactive `*.template.py` files from missing-pointer pressure, cleanup creates a timestamped backup before writing L1, and recall searches both compressed `all_histories.txt` and current JSON L4 sessions with sanitized excerpts.
- Runtime L4 recall is now conservative: history snippets are injected only for explicit history/continue/remember-like queries, and the injected block is marked as unverified recall that must be rechecked with tools before being treated as fact.
- `cargo run -q -p xtask -- memory-parity-smoke` now validates that upstream memory files are present and exercises audit/cleanup/recall in a non-mutating smoke path.
- L3 settlement guardrails now reject broad rewrites, code-block dumps, overlong updates, and unverified/guess-like notes before writing SOP/helper memory files.
- OCR/Vision helpers now mirror upstream `ocr_utils.py` / `vision_api.template.py`: RapidOCR/Tesseract OCR, CJK-space cleanup, bbox/conf details, `ocr_screen`/`ocr_window` Python bridges, no-fullscreen vision guardrails, OpenAI/Responses/Claude/MiMo vision calls, and `memory/vision_api.py` for `code_run` imports. `code_run` now injects the workspace memory path so Python snippets can directly `import ocr_utils` or `from vision_api import ask_vision` like upstream SOPs.
- Python runtime strategy is documented in `docs/python-runtime-strategy.md`: central resolver, cross-platform managed venv, `doctor`, `bootstrap-python`, and no-Python/full-parity gates. Phase 1 has shared resolver usage in `code_run`, reflect scripts, memory Python helpers, and `koda-agent doctor --json`; Phase 2 has managed-venv `koda-agent bootstrap-python` with core/extras requirement-file handling, `--repair`, and safe `python-env remove`.

Remaining priority:

1. `bootstrap-python` now has dry-run and offline missing-venv tests that prove it does not create a venv or touch network in those paths; optional uv behavior is controlled with `KODA_BOOTSTRAP_DISABLE_UV`.
2. Reflect mode hardening now covers atomic state writes, corrupt-state backups for goal/agent-team state, and agent-team poll failures being logged/skipped rather than aborting the loop. Remaining work is a longer real-service failure/recovery soak.

### Phase 5 - Browser Bridge Parity

Status: Partial/P1.

Implemented:

- Rust TMWebDriver-compatible master: WS and HTTP longpoll.
- Runtime-generated `assets/tmwd_cdp_bridge/config.js`.
- Extension null-safety fixes and CDP fallback.
- `xtask browser-smoke` for Chrome CDP.
- `xtask tmwd-extension-smoke` / `make smoke-tmwd-extension` for installed Edge/Chrome extension verification; validated against Edge with `tabs`, `management.list`, self-disable protection, `cdp Runtime.evaluate`, redacted `cookies`, and optional `contentSettings` allow + restore-to-ask mutation. `xtask tmwd-real-matrix` / `make smoke-tmwd-matrix` adds a live local-page matrix covering navigation, DOM form action, same-origin iframe read, cross-origin iframe blocked-access error shape, download request delivery, autofill/password-field candidate detection, target=_blank new-tab reporting when browser popup policy allows it, CDP `Runtime.evaluate`, CDP `Page.captureScreenshot`/fallback, batch `$N.path` references, cookies, management metadata, self-disable guard, and optional contentSettings mutation. The WS port now answers the extension's plain HTTP liveness probe like the Python master. Upstream already ships `management list|reload|disable|enable` and `contentSettings`; Rust intentionally adds `isSelf`/`mayDisable` metadata plus `confirmSelf=true` protection before disabling the bridge extension itself.
- `xtask tmwd-static-parity-smoke` / `make smoke-tmwd-static-parity` compares the Rust bridge assets/master against upstream `background.js`, `content.js`, and `TMWebDriver.py` for command surface parity: `cookies`, `cdp`, `batch`, `tabs`, `management`, `contentSettings`, management `list/reload/disable/enable`, `/link`, `/api/longpoll`, `/api/result`, and the Rust-only safety/fallback extensions.
- The DOM content bridge now routes extension-only `management` and `contentSettings` commands as well as `cookies`/`tabs`/`cdp`/`batch`, so both direct JSON-command and TID DOM bridge paths cover the upstream command surface.
- `web_execute_js` rich-monitor output is closer to upstream `simphtml.execute_js_rich()`: JS errors now return `status: "failed"` with `error` while still reporting monitor fields, no-change diffs use `DOM变化量: 0 (页面无变化)`, and changed-region detection now compares HTML element signatures rather than only line ranges.
- `xtask rich-monitor-smoke` / `make smoke-rich-monitor` creates a local CDP tab and validates the `web_execute_js` rich-monitor paths for no-change, DOM changed, transient text, JS error, new tab, and reload/navigation without network or `.env`.

Remaining priority:

1. Real-page rich monitor smoke for richer SPA interactions against Edge.
2. Provider-specific autofill/password-manager prompts and browser download shelf UI behavior.
3. Keep the static bridge parity smoke in CI-like local gates whenever upstream bridge assets are refreshed.

New fixture coverage:

- Reflect mode prompt/state fixtures for native scheduler, goal, autonomous, and agent-team worker.
- Phase 3 tool fixtures for `file_read` no-line-number/SOP prompts, `file_patch` error/success shapes, and `file_write` `<file_content>` fallback/blank rejection.
- Phase 5 browser bridge fixtures for JSON command detection, batch `$N.path` refs, extension-only API errors, nested batch rejection, master error normalization, and rich DOM diff/no-change monitor text.
- Phase 5 static parity smoke for upstream bridge command/method/route coverage and Rust-only safety/fallback extensions.

### Phase 6 - Frontend Parity

Status: Partial/P2.

Implemented:

- CLI, task mode, reflect mode, TUI skeleton, ACP JSONL, HTTP/web UI, Telegram basic, webhook scaffolds.
- ACP bridge now matches upstream content block conversion for text/resource_link/resource/image/unsupported blocks, upstream-style `sessionUpdate` event payloads, `genericagent-acp` initialize metadata, required `cwd` for `session/new`, unsupported `session/list`/`session/load`, and empty `session/close`.
- ACP prompt handling now keeps the JSONL loop responsive while a prompt is active, rejects a second prompt on the same session with `-32603 session already has an active prompt`, and `session/cancel`/`session/stop` call runtime abort like the Python bridge.
- `xtask acp-client-smoke` now exercises an external JSONL client process across success, running, and error boundaries: initialize, new, prompt streaming update, unsupported list/load, idle cancel, concurrent prompt rejection while a slow prompt is active, cancel-during-running, unknown session, invalid prompt shape, unknown method, invalid request, close, and shutdown.
- TUI panel is closer to the upstream Textual mental model: command strip, session sidebar, active-session stats, `/rename`, `/tail [n]`, `/view <start> [n]`, `/search <keyword>`, and `/panel` redraw are implemented while keeping the lightweight Rust line-mode fallback. A hidden full-screen Ratatui preview (`tui --full`) now establishes the future sidebar/timeline/inspector/composer layout, can run real tasks through async `AgentEvent` updates, and supports core session operations (`/branch`, `/switch`, `/rename`, `/clear`, `/close`) without changing the default line-mode entrypoint.

Remaining priority:

1. ACP interoperability with an actual codeg/ACP host beyond the local JSONL client smoke.
2. TUI mouse/full-screen Textual-like rendering and concurrent live session display.
3. ~~Telegram streaming edit, inline buttons, file/image support.~~ **Done** (51 tests, Phases A-E, pure text output). Remaining: Feishu/WeCom/DingTalk encrypted callback + file/reply parity.
4. Feishu/WeCom/DingTalk encrypted callback + file/reply parity (next IM target).
5. QQ/WeChat/native desktop/Qt/Streamlit/desktop pet feature-gated ports.

## Next Work Order

1. ~~Extend Phase 5 from automated matrix into selected real-site manual cases: provider-specific autofill/password-manager prompts, download shelf UI behavior, and complex cross-origin iframe workflows.~~ **Phase 2 LLM core parity is now Done/P1** with profile-based configuration completed.
2. Add ACP protocol fixture comparison and improve TUI layout/session display.
3. ~~Continue IM/frontend breadth: Telegram streaming/buttons/files, Feishu/WeCom/DingTalk encrypted callbacks and files.~~ **Telegram Done** (Phases A-E, 51 tests, pure text output). Next: Feishu/WeCom/DingTalk encrypted callbacks and file parity.
4. Add optional tracing integration and final real-LLM smoke matrix.
5. Update Phase 5 browser bridge with broader manual website edge cases (provider-specific autofill, download shelf UI).

---

## Document Maintenance

**Last updated**: 2026-05-13  
**Updated by**: Automatic review & manual update  
**Changes made**:
- Phase 2 (LLM Core): `Partial` → `Done/P1` — Profile-based LLM configuration (`config/llms.toml`) implemented with layered profile→model resolution
- Phase 6 (TUI): Added ToggleMouseCapture (F7/Ctrl-M) and improved `ask_user` interaction to feature list
- Telegram (IM): `Partial/P2` → Telegram portion **Done** — Phase A: send_tg_md2 + extract_feishu/wecom_json_text; Phase B: StreamSession + TurnStreamCoordinator; Phase C: command routing (/abort /continue /btw /debug) + ask_user inline buttons (7 functions); Phase D: file handling pipeline + proxy + UserState. **51 unit tests**, 0 failures. Pure text output (no MarkdownV2/HTML conversion). Feishu/WeCom/DingTalk remain Partial/P2.

**Next review due**: 2026-05-18 (or after significant feature merges)

## Current Non-Goals / Intentional Differences

- Rust uses `.env` and `config/llms.toml` as the primary secret source; legacy `mykey.py`/`mykey.json` is compatibility fallback only.
- Heavy platform frontends should remain behind Cargo features or separate crates.
- Python is not required for core runtime; it remains optional for compatibility scripts and executing user Python code.
