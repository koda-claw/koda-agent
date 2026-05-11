# Release Notes

## v0.1.3

- LLM config is now `llms.toml` profile-first. Use `koda-agent config setup <preset>`, `config secret`, `config list`, `config show`, `config use`, `config add`, `config set`, `config remove`, and `config validate` instead of editing `.env` by hand.
- Added `config migrate` for legacy `OPENAI_*` environments. It creates an `openai-compat` profile and keeps the real key in `.env`; secrets are not copied into TOML.
- Added provider auth controls with `auth_scheme` and `auth_header`, including MiMo-compatible `api-key` header auth without sending an extra Bearer token.
- Added process-level `--profile <name>` and `--llm-no` alias, plus runtime `/llm <profile-name>` switching.
- Full-screen TUI Inspector now shows the active profile separately from the model.
- Install/release packaging now includes config templates and initializes the user home layout without committing local runtime config.

Known migration note: legacy `OPENAI_BASE_URL` / `OPENAI_MODEL` fallback is no longer the recommended startup path. If those variables are detected without `config/llms.toml`, run `koda-agent config migrate` or `koda-agent config setup mimo`.
