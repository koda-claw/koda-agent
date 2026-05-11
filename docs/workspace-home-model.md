# Workspace, Home, and Resource Model

## Goal

Koda Agent must work as an installed CLI, not only as a source checkout. After
installation, users should get a stable user-space directory similar to
`.claude`, while agent file tools continue to operate on the current project
workspace.

This document defines the target path model before the first public release.

## Current State

The current implementation treats the process current directory as `root_dir`:

```text
root_dir   = current working directory
temp_dir   = root_dir/temp
memory_dir = root_dir/memory
logs_dir   = root_dir/logs
assets     = root_dir/assets
config     = root_dir/config/llms.toml or root_dir/.env
```

This is convenient for development, but it is not correct for installed binary
usage:

- a release binary can run outside the source repository;
- runtime memory, logs, and temp files may pollute user project directories;
- `assets/`, tool schemas, system prompts, memory SOPs, browser bridge assets,
  and Python helper files may be missing when the source checkout is absent;
- browser bridge `config.js` is runtime data and should not be generated into a
  source-controlled asset directory;
- users expect one durable home such as `~/.koda-agent` for state and config.

## Target Model

Koda Agent uses three distinct roots.

### 1. Koda Home

Koda Home is the durable user-space runtime directory.

Default paths:

```text
macOS/Linux: ~/.koda-agent
Windows:     %USERPROFILE%\.koda-agent
```

Override:

```text
KODA_AGENT_HOME=/custom/path
```

Target structure:

```text
~/.koda-agent/
  config/
    .env
    llms.toml
  memory/
    global_mem.txt
    global_mem_insight.txt
    file_access_stats.json
    long_term_updates.jsonl
    L4_raw_sessions/
  logs/
    llm_usage.jsonl
    langfuse_trace.jsonl
  temp/
    model_responses/
    reflect_logs/
    _stop_signal
  sessions/
  python/
    venv/
  resources/
    assets/
    memory/
    requirements-python-core.txt
    requirements-python-ocr.txt
    requirements-python-automation.txt
    requirements-python-dev.txt
  browser/
    tmwd_cdp_bridge/
      config.js
```

`Koda Home` owns mutable runtime state. It is never committed to a project
repository.

### 2. Workspace

Workspace is the project or task directory that the agent operates on.

Default:

```text
workspace = process current directory
```

Overrides, in priority order:

```text
--workspace /path/to/project
KODA_WORKSPACE=/path/to/project
```

Workspace is used for user-visible file tools and command execution:

- `file_read`
- `file_write`
- `file_patch`
- default `code_run cwd`
- task input/output directories when the user intentionally starts task mode for
  a project

Rules:

- relative paths such as `src/main.rs` resolve under workspace;
- root-like paths such as `/matrix.html` resolve to `workspace/matrix.html`, not
  the OS filesystem root;
- explicit absolute OS paths remain absolute only when they are not root-like
  pseudo paths and pass future safety policy;
- runtime files such as `_stop_signal`, model response logs, memory archives,
  and usage logs live in Koda Home, not workspace.

### 3. Resources

Resources are read-only or replaceable distribution assets required by the
runtime.

Resolution order:

1. `KODA_RESOURCE_DIR`, if set.
2. a packaged `resources/` directory next to the executable or install manifest.
3. source checkout resources, only when a checkout is detected.
4. `<KODA_AGENT_HOME>/resources`, populated from packaged/source resources.
5. compiled-in fallback for critical small assets, if implemented later.

Resource contents:

```text
assets/sys_prompt*.txt
assets/tools_schema*.json
assets/simphtml_*.js
assets/tmwd_cdp_bridge/*
memory/*.md
memory/*.py
memory/skill_search/**
requirements-python-*.txt
```

Resource files may be refreshed by installer/update. User-owned memory and
config must not be overwritten by resource refresh. The packaged/source lookup
before home resources avoids stale `~/.koda-agent/resources` hiding fresh source
changes during development, while still making installed binaries independent of
source code after resources are copied into Koda Home.

## Configuration Loading

Configuration must support both project-local and user-global setups.

Recommended effective priority:

1. environment variables already exported by the shell;
2. workspace `.env` for project-local credentials/config;
3. `KODA_AGENT_HOME/config/.env` for user-global credentials/config;
4. workspace `config/llms.toml`;
5. `KODA_AGENT_HOME/config/llms.toml`;
6. legacy workspace `mykey.json` / `mykey.py`;
7. legacy home `config/mykey.json` / `config/mykey.py`, optional compatibility.

Implementation note: this priority should not depend on global process-env
mutation order. `dotenvy::from_path()` does not override existing variables, so
loading multiple `.env` files in the wrong order can silently invert the desired
priority. The implementation should parse config files into maps and merge them
explicitly, or load from lowest to highest priority with intentional override
semantics.

Rationale:

- direct environment variables remain the most explicit;
- project `.env` preserves the current developer workflow;
- home `.env` gives installed users a stable default;
- local `config/llms.toml` can override user-global model routing for a project;
- legacy files are compatibility fallbacks, not the preferred public path.

Secrets must keep the existing redaction behavior in logs and diagnostics. Home
config files containing secrets should be created with user-only permissions on
Unix-like systems when Koda Agent writes them.

## Runtime Path Mapping

`AgentConfig` should evolve from one `root_dir` into explicit fields.

Proposed fields:

```rust
pub struct AgentPaths {
    pub home_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub resource_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub memory_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub browser_dir: PathBuf,
}
```

Compatibility strategy:

- keep `AgentConfig.root_dir` temporarily as an alias for `resource_dir` or
  source-compatible lookup during migration;
- introduce accessors such as `cfg.workspace_dir()` and `cfg.resource_asset()`;
- migrate internal callers in phases;
- remove ambiguous `root_dir` usage after tests cover the new model.

Initial mapping:

```text
cfg.home_dir      = resolved Koda Home
cfg.workspace_dir = resolved workspace
cfg.resource_dir  = resolved resources
cfg.temp_dir      = home_dir/temp
cfg.memory_dir    = home_dir/memory
cfg.logs_dir      = home_dir/logs
cfg.sessions_dir  = home_dir/sessions
cfg.browser_dir   = home_dir/browser
```

Session state should be keyed by workspace identity so histories from different
projects do not blend accidentally. A normalized workspace path hash is enough
for the first pass.

Python helper environments should move under `home_dir/python/venv`. The current
platform data-dir default can remain as a legacy fallback during migration, but
`KODA_AGENT_HOME` must become the primary source of truth.

Task mode needs one explicit rule before implementation: relative `--task NAME`
should use `home_dir/temp/NAME` for background runtime I/O, while absolute or
workspace-qualified task directories should remain user-controlled. This keeps
background temp files out of project roots by default.

## Resource Bootstrap

A first-run/bootstrap step must ensure required files are available under
Koda Home.

Bootstrap actions:

1. create home directories;
2. copy resource assets from installed package/source checkout to
   `home/resources` when missing;
3. copy memory templates/SOPs to `home/resources/memory`, not to user memory;
4. initialize mutable memory files in `home/memory` only when missing;
5. generate browser bridge runtime config under `home/browser/tmwd_cdp_bridge`;
6. never copy `.env` automatically;
7. never overwrite user-owned `home/config`, `home/memory`, `home/logs`, or
   `home/temp` without explicit repair/reset flags.

Installer should also be able to refresh resources during update:

```text
koda-agent resources install --source <package-resources> --repair
koda-agent resources doctor
```

These commands may be implemented later; the first pass can perform equivalent
logic inside `ensure_dirs()` and installer scripts.

## Browser Bridge Model

The unpacked extension currently lives under source `assets/tmwd_cdp_bridge`.
Installed usage should use two directories:

```text
~/.koda-agent/resources/assets/tmwd_cdp_bridge/ # pristine packaged copy
~/.koda-agent/browser/tmwd_cdp_bridge/          # user-loaded unpacked extension copy
~/.koda-agent/browser/tmwd_cdp_bridge/config.js # generated runtime config
```

Self-review correction: generated `config.js` should not live in the pristine
`resources/` tree if resources are treated as refreshable package content. The
first pass should copy static extension files from resources into
`home/browser/tmwd_cdp_bridge`, generate `config.js` there, and instruct users to
load that copied extension directory in Edge/Chrome.

## Install Package Contents

Release archives should include more than the binary.

Recommended archive layout:

```text
koda-agent
resources/
  assets/
  memory/
  requirements-python-core.txt
  requirements-python-ocr.txt
  requirements-python-automation.txt
  requirements-python-dev.txt
README.md
LICENSE
```

The installer should:

1. install binary into the selected prefix, for example `~/.local/bin`;
2. install/copy `resources/` into `~/.koda-agent/resources`;
3. create `~/.koda-agent/config`, `memory`, `logs`, `temp`, `sessions`,
   `browser`;
4. run `koda-agent doctor --json` as a smoke check when possible;
5. optionally run `koda-agent bootstrap-python --extras core --repair` when the
   user requests Python helper support.

## CLI Surface Changes

Add global options:

```text
koda-agent --workspace <DIR> ...
koda-agent --home <DIR> ...
koda-agent --resource-dir <DIR> ...
```

Environment equivalents:

```text
KODA_WORKSPACE
KODA_AGENT_HOME
KODA_RESOURCE_DIR
```

Doctor output should include:

```json
{
  "paths": {
    "home_dir": "...",
    "workspace_dir": "...",
    "resource_dir": "...",
    "temp_dir": "...",
    "memory_dir": "...",
    "logs_dir": "..."
  },
  "resources": {
    "sys_prompt": true,
    "tools_schema": true,
    "tmwd_cdp_bridge": true,
    "memory_sops": true
  }
}
```


## Non-Goals for the First Pass

The first pass should not introduce multi-user server semantics. `~/.koda-agent`
is a single-user local home. Team/server deployment can later map the same
concept to an explicit service data directory.

The first pass should not move IM/GUI frontend implementation forward. It only
prepares the path model that those frontends can reuse later.

The first pass should not require Python. Python remains an optional helper
capability managed under Koda Home when requested.

## Success Criteria

A release candidate is acceptable only when all of the following are true:

- `koda-agent doctor --json` works from a random directory outside the source
  checkout after installation.
- `doctor --json` reports home, workspace, resource, temp, memory, and logs
  paths.
- `file_write` and `file_patch` modify the workspace, not `~/.koda-agent`.
- runtime logs, model response logs, L4 sessions, task temp directories, and
  stop-signal files are created under Koda Home by default.
- system prompt, tool schema, HTML simplification assets, memory SOP resources,
  and browser extension assets are available without the source checkout.
- browser extension users load the home-managed extension copy, and generated
  `config.js` is never written into the source checkout.
- home config permissions are user-private when Koda Agent creates secret-bearing
  files on Unix-like systems.
- installer dry-run and temp-prefix install tests pass on macOS/Linux, and
  PowerShell dry-run/parsing passes in Windows CI.
- existing source-checkout developer flow still works with workspace `.env`.

## Migration Plan

### Phase A: Path Resolver

- Add `AgentPaths` and `resolve_agent_paths()`.
- Keep existing `AgentConfig::from_env(root)` API temporarily, but route it
  through the new resolver.
- Add tests for macOS/Linux/Windows-like path decisions without requiring those
  platforms.
- Add explicit config merge tests so environment, workspace `.env`, and home
  `.env` priority cannot regress.

Acceptance:

```bash
cargo test -p koda-agent-core paths
cargo test --workspace --all-features
```

### Phase B: Workspace-Safe Tools

- Change `GenericToolDispatcher.cwd` default from `temp_dir` to
  `workspace_dir`.
- Keep runtime control files in `temp_dir`.
- Ensure `file_write("/x")` maps to `workspace/x` cross-platform.
- Ensure `code_run` default cwd is workspace, not home temp.

Acceptance:

- file tool tests prove workspace writes;
- runtime stop-signal tests still use home temp;
- Windows CI remains green.

### Phase C: Resource Lookup

- Replace direct `root_dir/assets/...` reads with resource lookup helpers.
- Replace direct `root_dir/memory/...` template/SOP reads with resource lookup
  for static files and `memory_dir` for mutable files.
- Copy browser bridge assets into the home-managed extension directory and
  generate config there.

Acceptance:

- running from a temp directory without source `assets/` still passes minimal
  `doctor` and tool schema/system prompt load tests;
- `make smoke-tmwd-static-parity` still passes in source checkout.

### Phase D: Installer and Release Packaging

- Package `resources/` into release archives.
- Make `install.sh` / `install.ps1` install resources into Koda Home.
- Add dry-run and real temp-prefix install smoke tests.
- Update `release.yml` archive layout and checksums.

Acceptance:

```bash
make release-dry-run
KODA_AGENT_HOME=/tmp/koda-home-test scripts/install.sh --from-source --prefix /tmp/koda-prefix-test
/tmp/koda-prefix-test/bin/koda-agent doctor --json
```

### Phase E: Documentation and Compatibility

- Update README install section.
- Update `docs/installation.md` with `.koda-agent` layout.
- Update `docs/configuration.md` with config priority.
- Update `docs/browser-extension.md` with installed extension path.
- Mention legacy source-checkout behavior remains supported for contributors.

## Risks and Mitigations

Risk: breaking source checkout development.

Mitigation: keep workspace `.env`, workspace config, and source resources as
fallbacks.

Risk: resource refresh overwrites user memory.

Mitigation: resources live under `home/resources`; mutable memory lives under
`home/memory`; never overwrite mutable user files without explicit reset.

Risk: browser extension cannot import config from outside extension directory.

Mitigation: copy extension static files into Koda Home and generate `config.js`
there. Users load the copied extension path, not the source asset path.

Risk: hidden path behavior surprises users.

Mitigation: show paths in `doctor --json`, TUI inspector, and startup/status
messages.

Risk: too much refactor before release.

Mitigation: phased migration, keep `root_dir` compatibility alias, and require
full local + GitHub CI after every phase.

## Self Review

The design separates mutable user state from project files and packaged static
resources. This matches the expected `.claude`-style mental model and avoids
source-checkout assumptions in release binaries.

Review finding 1: the first draft made `resources/` sound both read-only and
mutable for browser `config.js`. That is inconsistent. The corrected design keeps
pristine package resources in `home/resources` and creates a mutable, user-loaded
extension copy in `home/browser/tmwd_cdp_bridge`.

Review finding 2: config priority cannot rely on repeated `dotenvy::from_path()`
loads, because non-overwrite behavior can invert precedence. The implementation
must parse and merge config layers deliberately.

Review finding 3: installed packages need an adjacent packaged `resources/`
directory. Looking only in `~/.koda-agent/resources` and source checkout is not
sufficient for first-run install, resource repair, or release archive testing.

Review finding 4: Python helper venv and task-mode temp directories were
under-specified. The corrected model puts optional Python helpers under
`home/python/venv` and default relative task I/O under `home/temp/<task>`.

The main compatibility concern remains existing code that treats `root_dir` as
both resource root and workspace root. The plan handles this by adding explicit
path fields first, then migrating callers gradually rather than performing a
large rename-only refactor.

The release should not be tagged before Phases A-D are complete, because a
binary-only archive would otherwise work only from source checkouts and would not
match user expectations for an installed CLI.
