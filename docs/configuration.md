# Configuration

Koda Agent separates runtime home, workspace, and packaged resources:

- `KODA_AGENT_HOME` or `--home`: runtime home, default `~/.koda-agent`.
- `KODA_WORKSPACE` or `--workspace`: file-tool workspace, default current directory.
- `KODA_RESOURCE_DIR` or `--resource-dir`: packaged/source resources.

## LLM Configuration

The product direction is `llms.toml` first:

```text
~/.koda-agent/config/llms.toml       # model/provider profiles
~/.koda-agent/config/llms.example.toml # template/reference only
~/.koda-agent/.env                   # secrets, KODA_LLM_PROFILE, KODA_LLM_MODEL
```

`koda-agent init` creates both files on a clean home: `llms.toml` is the active
runtime config, while `llms.example.toml` is kept as a richer template. It also
installs static resources into `~/.koda-agent/resources` from the resolved
resource source when available. If `init` copies a legacy `.env` with
`OPENAI_BASE_URL` and `OPENAI_MODEL`, it creates an `openai-compat` profile and
keeps `OPENAI_API_KEY` in `.env`.

The normal user path is CLI setup, not manual editing:

```bash
koda-agent config setup mimo --yes
koda-agent config secret MIMO_API_KEY --from-stdin
koda-agent config validate
koda-agent tui --full
```

`config setup` writes provider profiles and model aliases to `llms.toml` and stores API keys in
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
koda-agent config model list deepseek
koda-agent config model use deepseek flash
koda-agent config model remove deepseek flash --force
koda-agent --profile deepseek --model flash --input "hi"
koda-agent --llm deepseek:flash --input "hi"
koda-agent config setup glm --api-key-env ZHIPUAI_API_KEY --yes
koda-agent --llm glm:default --input "hi"
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
KODA_LLM_MODEL=pro
MIMO_API_KEY=...
```

Example `llms.toml`:

```toml
[selector]
default_profile = "mimo"
default_model = "pro"

[[profiles]]
name = "mimo"
kind = "native_oai"
base_url = "https://api.xiaomimimo.com/v1"
api_key_env = "MIMO_API_KEY"
auth_scheme = "header"
auth_header = "api-key"
api_mode = "chat_completions"
stream = true

[[profiles.models]]
name = "pro"
id = "mimo-v2.5-pro"
```

Legacy `OPENAI_BASE_URL` / `OPENAI_MODEL` values are migration inputs. New runtime config uses `llms.toml` with `[[profiles.models]]`; profile-level `model = "..."` is rejected with a migration hint.

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
