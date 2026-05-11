# Installation

Koda Agent supports three installation modes: release binaries for normal users,
source installs for contributors, and an optional managed Python helper runtime
for GenericAgent-compatible Python scripts.

## macOS and Linux

Release install, once a GitHub repository is configured:

```bash
curl -fsSL https://raw.githubusercontent.com/<owner>/<repo>/main/scripts/install.sh \
  | KODA_AGENT_REPO=<owner>/<repo> sh
```

Source install from a checked-out repository:

```bash
scripts/install.sh --from-source
```

The installer also populates `~/.koda-agent/resources` from the checkout or
release archive. This keeps packaged prompts, tool schemas, memory SOPs, Python
requirements, and browser bridge assets separate from your project workspace.

Install to a custom prefix:

```bash
scripts/install.sh --from-source --prefix ~/.local
```

Add the install prefix to `PATH` if needed:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

## Windows PowerShell

Release install, once a GitHub repository is configured:

```powershell
iwr https://raw.githubusercontent.com/<owner>/<repo>/main/scripts/install.ps1 -UseB | iex
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
~/.koda-agent/resources installed resources copied by installers
```

Override locations when needed:

```bash
koda-agent --home /path/to/home --workspace /path/to/project doctor
KODA_AGENT_HOME=/path/to/home KODA_WORKSPACE=/path/to/project koda-agent doctor
```

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

macOS/Linux:

```bash
scripts/update.sh --repo <owner>/<repo>
```

Windows:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/update.ps1 -Repo <owner>/<repo>
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
