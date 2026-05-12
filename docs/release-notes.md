# Release Notes

## v0.1.7

- Improved full-screen TUI session naming: default sessions are auto-named from the first user prompt, historical sessions prefer their first user prompt over timestamp-only names, and the Sessions sidebar keeps more readable title width.
- Hardened full-screen TUI historical tool rendering by parsing Chinese/English tool-call summaries plus structured `[ToolCall]` / `[ToolResult]` lines back into tool cards.
- Unified session activation behavior so keyboard, mouse, slash-command, and close fallback switches clear unread/unseen counters consistently.

## v0.1.5

- Added `koda-agent goal` as a first-class Goal Mode entrypoint. It creates the upstream-compatible `goal_state.json`, starts the native `goal_mode` reflect loop, supports `--budget`, `--max-turns`, `--state`, `--resume`, `--dry-run`, and `--json`, and keeps the lower-level `GOAL_STATE=... koda-agent --reflect goal_mode` path available.
- Documented Goal Mode usage in the Chinese CLI manual.

## v0.1.4

- Updated default LLM templates for current provider surfaces: DeepSeek now only ships `deepseek-v4-pro` / `deepseek-v4-flash`, OpenAI defaults to GPT-5 family aliases, and GLM/BigModel is available through `config setup glm`.
- Increased default Agent-friendly generation budgets and timeouts in `llms.example.toml` while keeping provider-specific caps below maximum limits for safer cost and latency.
- Added Chinese CLI/tutorial documentation with an mdBook source tree plus `make docs` / `make docs-serve`.
- CLI help now includes descriptions for top-level and nested commands.
- Hardened resource/memory resolution, static resource installation during init, and code-run secret-like output redaction.

- LLM config is now `llms.toml` profile/model-first: profiles represent provider endpoints and `[[profiles.models]]` defines model aliases. Use `koda-agent config setup <preset>`, `config secret`, `config list`, `config show`, `config use`, `config model list/add/set/use/remove`, `config add`, `config set`, `config remove`, and `config validate` instead of editing `.env` by hand.
- Added `config migrate` for legacy `OPENAI_*` environments. It creates an `openai-compat` profile and keeps the real key in `.env`; secrets are not copied into TOML.
- Added provider auth controls with `auth_scheme` and `auth_header`, including MiMo-compatible `api-key` header auth without sending an extra Bearer token.
- Added process-level `--profile <name> --model <alias>`, `--llm <profile:model>`, and `--llm-no` alias, plus runtime `/llm <profile:model>`, `/models`, and `/model <alias>` switching.
- Full-screen TUI Inspector now shows the active profile separately from the model.
- `koda-agent init` now installs static resources into `~/.koda-agent/resources` when a packaged/source resource root is available, so `resources doctor` no longer reports home markers missing after init-only setup.
- Install/release packaging now includes config templates and initializes the user home layout without committing local runtime config.

Known migration note: legacy `OPENAI_BASE_URL` / `OPENAI_MODEL` fallback is no longer the recommended startup path. If those variables are detected without `config/llms.toml`, run `koda-agent config migrate` or `koda-agent config setup mimo`.
