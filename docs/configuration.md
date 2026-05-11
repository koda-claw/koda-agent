# Configuration

Koda Agent separates runtime home, workspace, and packaged resources:

- `KODA_AGENT_HOME` or `--home`: runtime home, default `~/.koda-agent`.
- `KODA_WORKSPACE` or `--workspace`: file-tool workspace, default current directory.
- `KODA_RESOURCE_DIR` or `--resource-dir`: packaged/source resources.

## LLM Configuration

The product direction is `llms.toml` first:

```text
~/.koda-agent/config/llms.toml       # model/provider profiles
~/.koda-agent/config/llms.example.toml
~/.koda-agent/.env                   # secrets and KODA_LLM_PROFILE
```

The normal user path is CLI setup, not manual editing:

```bash
koda-agent config setup mimo --yes
koda-agent config secret MIMO_API_KEY --from-stdin
koda-agent config validate
koda-agent tui --full
```

`config setup` writes profile definitions to `llms.toml` and stores API keys in
`.env`. Real keys are not written into TOML by default.

Useful commands:

```bash
koda-agent config path
koda-agent config list
koda-agent config show mimo
koda-agent config setup mimo --from-env --yes
koda-agent config setup deepseek --api-key-env DEEPSEEK_API_KEY --yes
koda-agent config use deepseek
koda-agent config secret DEEPSEEK_API_KEY --from-stdin
koda-agent config set deepseek model deepseek-reasoner
koda-agent config remove deepseek
koda-agent config migrate
koda-agent config validate --json
```

`kind` mirrors upstream GenericAgent `mykey.py` session naming:

- `native_oai`: OpenAI-compatible native tool/function calling.
- `native_claude`: Anthropic Messages native tools.
- `oai`: OpenAI-compatible text tool protocol.
- `claude`: Claude Messages text tool protocol.

Example `.env`:

```bash
KODA_LLM_PROFILE=mimo
MIMO_API_KEY=...
```

Example `llms.toml`:

```toml
[selector]
default = "mimo"

[[profiles]]
name = "mimo"
kind = "native_oai"
base_url = "https://api.xiaomimimo.com/v1"
api_key_env = "MIMO_API_KEY"
auth_scheme = "header"
auth_header = "api-key"
model = "mimo-v2.5-pro"
api_mode = "chat_completions"
stream = true
```

Legacy `OPENAI_BASE_URL` / `OPENAI_MODEL` runtime fallback is being removed in
favor of `llms.toml`. Use the migration/setup flow instead of relying on those
variables as primary config.

## Vision Helpers

Optional multimodal helper variables are still environment-driven:

```bash
VISION_BASE_URL=https://api.example.com/v1/chat/completions
VISION_API_KEY=...
VISION_MODEL=...
VISION_API_KEY_HEADER=api-key
```

Secrets are redacted in logs. Do not commit `.env`, `config/llms.toml`, local
logs, browser runtime config, or memory runtime files.

Useful diagnostics:

```bash
koda-agent doctor --json
koda-agent resources doctor --json
```
