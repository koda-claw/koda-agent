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

The default Windows prefix is `%LOCALAPPDATA%\koda-agent`, and the script adds
its `bin` directory to the user `PATH`.

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
