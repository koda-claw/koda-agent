# Configuration

Koda Agent separates runtime home, workspace, and packaged resources:

- `KODA_AGENT_HOME` or `--home`: runtime home, default `~/.koda-agent`.
- `KODA_WORKSPACE` or `--workspace`: file-tool workspace, default current directory.
- `KODA_RESOURCE_DIR` or `--resource-dir`: packaged/source resources.

Koda Agent reads LLM configuration from the current directory, workspace, home,
and resource directory. Environment variables win over file configuration.
Supported files are:

1. `.env` for local or user-global credentials.
2. `config/llms.toml` for multi-model configuration.
3. Legacy `mykey.json` or `mykey.py` dictionaries for GenericAgent compatibility.

Required OpenAI-compatible variables:

```bash
OPENAI_BASE_URL=https://api.openai.com/v1
OPENAI_API_KEY=sk-...
OPENAI_MODEL=gpt-4.1-mini
```

Supported API styles:

- `OPENAI_API_STYLE=chat` for OpenAI-compatible Chat Completions.
- `OPENAI_API_STYLE=responses` for OpenAI Responses API wire shape.
- `OPENAI_API_STYLE=claude` for Anthropic `/v1/messages`.

Optional multimodal helper variables:

```bash
VISION_BASE_URL=https://api.example.com/v1/chat/completions
VISION_API_KEY=...
VISION_MODEL=...
VISION_API_KEY_HEADER=api-key
```

Secrets are redacted in logs. Do not commit `.env`, local logs, browser runtime
config, or memory runtime files.

Useful diagnostics:

```bash
koda-agent doctor --json
koda-agent resources doctor --json
```
