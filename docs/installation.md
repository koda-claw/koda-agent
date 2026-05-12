# Installation

Koda Agent supports three installation modes: release binaries for normal users,
source installs for contributors, and an optional managed Python helper runtime
for GenericAgent-compatible Python scripts.

## macOS and Linux

Release install from the official GitHub repository:

```bash
curl -fsSL https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.sh \
  | KODA_AGENT_REPO=koda-claw/koda-agent sh
```

Install a specific release tag:

```bash
curl -fsSL https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.sh \
  | KODA_AGENT_REPO=koda-claw/koda-agent KODA_AGENT_VERSION=v0.1.7 sh
```

Source install from a checked-out repository:

```bash
scripts/install.sh --from-source
```

The installer also populates `~/.koda-agent/resources` from the checkout or
release archive. This keeps packaged prompts, tool schemas, memory SOPs, Python
requirements, and browser bridge assets separate from your project workspace.
It also runs `koda-agent init`: if the current directory has a complete `.env`,
that file is copied into `~/.koda-agent/.env`; otherwise a local template is
created there for you to fill in. `init` creates the active runtime config at
`~/.koda-agent/config/llms.toml` and keeps the full provider template at
`~/.koda-agent/config/llms.example.toml`.

Install to a custom prefix:

```bash
scripts/install.sh --from-source --prefix ~/.local
```

Add the install prefix to `PATH` if needed:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

## Windows PowerShell

Release install from the official GitHub repository:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=$env:TEMP+'\koda-agent-install.ps1'; iwr -UseBasicParsing https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.ps1 -OutFile $s; & $s -Repo koda-claw/koda-agent"
```

Install a specific release tag:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=$env:TEMP+'\koda-agent-install.ps1'; iwr -UseBasicParsing https://raw.githubusercontent.com/koda-claw/koda-agent/main/scripts/install.ps1 -OutFile $s; & $s -Repo koda-claw/koda-agent -Version v0.1.7"
```

Source install from a checked-out repository:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/install.ps1 -FromSource
```

The default Windows prefix is `%LOCALAPPDATA%\koda-agent`, runtime data lives in
`%USERPROFILE%\.koda-agent`, and the script adds the prefix `bin` directory to
the user `PATH`.

## Runtime Home And Workspace

By default Koda Agent uses:

```text
~/.koda-agent/          runtime home: config, temp, memory, logs, sessions, browser
current directory       workspace: files read/written by tools
~/.koda-agent/resources installed resources copied by init/installers
```

Override locations when needed:

```bash
koda-agent --home /path/to/home --workspace /path/to/project doctor
KODA_AGENT_HOME=/path/to/home KODA_WORKSPACE=/path/to/project koda-agent doctor
```

Initialize or repair the runtime home without reinstalling:

```bash
koda-agent init
koda-agent init --from-env /path/to/.env
koda-agent init --force --from-env /path/to/.env
koda-agent config setup mimo --yes
koda-agent config secret MIMO_API_KEY --from-stdin
koda-agent config validate
```

`koda-agent init` is intentionally safe: it creates a default `mimo:pro` LLM in
`llms.toml` with an empty `MIMO_API_KEY`, or, when it copies a legacy
`OPENAI_*` `.env`, creates an `openai-compat:default` LLM without writing the key to
TOML. It also copies packaged/source resources into `~/.koda-agent/resources`
when available, without copying runtime files such as `.env`, `config.js`,
`llms.toml`, memory logs, or L4 session dumps. Use `config setup`,
`config secret`, or `config migrate --force` to refine the selected provider.

Runtime configuration lookup checks the current directory, the explicit
workspace, `~/.koda-agent/.env`, installed resources, and the platform config
directory such as `~/.config/koda-agent/.env`. Secrets are never printed by
`doctor` or `init`.

Use `~/.koda-agent/config/llms.toml` for provider profiles, per-profile model aliases, multi-model
failover, Claude Messages API, Responses API, timeout, proxy, and
reasoning/thinking settings. Keep real secrets in `.env`; `llms.example.toml` is
only a template/reference file.

Repair or inspect resources explicitly:

```bash
koda-agent resources install --repair
koda-agent resources doctor --json
```

## Optional Python Helpers

The Rust core does not require Python. Python is only needed for compatibility
helpers such as reflect scripts, OCR, vision helper scripts, and upstream SOPs
that import files under `memory/`.

Install or repair the managed helper environment:

```bash
koda-agent bootstrap-python --extras core --repair
```

Or ask the installer to bootstrap it:

```bash
scripts/install.sh --from-source --bootstrap-python
```

The managed environment is isolated from the system Python. Use:

```bash
koda-agent doctor --json
```

to inspect Python discovery, venv status, and runtime capability flags.

The default managed helper venv is `~/.koda-agent/python/venv` on macOS/Linux
and `%USERPROFILE%\.koda-agent\python\venv` on Windows. Existing legacy app-data
venvs are still considered during discovery, but new bootstrap/remove operations
target the Koda home venv.

## Update

Installed users can update directly with the binary. This path is preferred
because it works without a checked-out repository:

```bash
koda-agent update --check
koda-agent update --check --json
koda-agent update --repo koda-claw/koda-agent --version latest
koda-agent update --repo koda-claw/koda-agent --version v0.1.7
```

`--check` queries GitHub's latest release metadata, compares it with the
installed CLI version printed by `koda-agent --version`, and reports whether an
update is available without downloading or replacing anything.

`koda-agent update` detects the current platform, downloads the matching GitHub
Release asset, verifies `SHA256SUMS`, replaces the installed binary, and repairs
`~/.koda-agent/resources` unless `--no-resources` is passed. Supported release
targets are:

```text
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
x86_64-apple-darwin
aarch64-apple-darwin
x86_64-pc-windows-msvc
aarch64-pc-windows-msvc
```

Use `--prefix <dir>` when the binary should be installed under a specific
prefix instead of replacing the currently running executable's directory:

```bash
koda-agent update --repo koda-claw/koda-agent --prefix ~/.local
```

The legacy script wrappers are still available and delegate to the installer.

macOS/Linux script wrapper:

```bash
scripts/update.sh --repo koda-claw/koda-agent
```

Windows script wrapper:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/update.ps1 -Repo koda-claw/koda-agent
```

## Uninstall

macOS/Linux:

```bash
scripts/uninstall.sh
scripts/uninstall.sh --remove-data
```

Windows:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/uninstall.ps1
powershell -ExecutionPolicy Bypass -File scripts/uninstall.ps1 -RemoveData
```

`--remove-data` deletes user data/config directories. It does not touch source
checkouts or Git repositories.
