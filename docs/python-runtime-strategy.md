# Python Runtime Strategy

## Objective

Koda Agent's Rust runtime must stay usable without Python, while the GenericAgent-compatible Python helper ecosystem remains first-class when available. Python should be treated as a managed optional capability: detected centrally, installed into an isolated environment when requested, and reported clearly when a tool needs it but it is missing.

This plan covers macOS, Linux, and Windows. It avoids global `pip install`, avoids shell-specific activation scripts, and keeps secrets out of diagnostics.

## Current Touchpoints

Python is currently resolved in several places:

- `crates/koda-agent-tools/src/lib.rs`: `code_run` uses `KODA_PYTHON`, then `python3` / `python`.
- `crates/koda-agent-cli/src/main.rs`: Python reflect scripts use local `find_python_interpreter`.
- `crates/koda-agent-memory/src/lib.rs`: OCR/Vision helpers use `KODA_PYTHON`, `PYTHON`, then `python3` / `python`.
- `crates/xtask/src/main.rs`: `vision-smoke` generates a sample image through `python3` + Pillow.
- `memory/*.py` and `assets/code_run_header.py`: compatibility helpers kept to match upstream GenericAgent SOPs.

The immediate problem is duplicated resolver logic and inconsistent missing-Python messages. The target is one shared resolver and one doctor/bootstrap surface.

## Design Principles

- Rust core must not fail startup only because Python is absent.
- Python-dependent operations must fail with actionable structured errors.
- Koda-managed dependencies must live in a dedicated venv, not global site-packages.
- Do not require `activate`; always invoke the venv interpreter directly.
- Use Rust `Command` with separate args; never build shell command strings for paths.
- Use platform app-data directories by default, with env/workspace overrides.
- Keep upstream compatibility: do not delete Python helper files or SOP references.
- Installation must be explicit via CLI, not automatic during normal agent execution.

## Target User Commands

```bash
koda-agent doctor
koda-agent doctor --json
koda-agent bootstrap-python
koda-agent bootstrap-python --extras ocr
koda-agent bootstrap-python --extras automation
koda-agent bootstrap-python --extras all
koda-agent bootstrap-python --recreate
koda-agent bootstrap-python --repair
koda-agent python-env remove
```

`doctor` is non-mutating. `bootstrap-python` is the only command that creates venvs or installs packages.
`python-env remove` removes only the managed venv after verifying the path is the managed directory.

## Runtime Resolution Order

The shared resolver should return either `PythonRuntime` or `PythonUnavailable`. It should be purpose-aware because project code execution and agent helper execution have different expectations.

For agent helpers (`ocr_utils.py`, `vision_api.py`, reflect compatibility helpers):

1. `KODA_PYTHON`, if set and executable.
2. Legacy `PYTHON`, if set and executable.
3. Managed venv interpreter.
4. Workspace `.koda/python/venv` interpreter.
5. Workspace `.venv` interpreter.
6. System candidates: `python3`, `python`.
7. Windows only: `py -3` launcher.
8. Unavailable.

For user Python snippets in `code_run`:

1. `KODA_PYTHON`, if set and executable.
2. Workspace `.venv` interpreter.
3. Workspace `.koda/python/venv` interpreter.
4. Managed venv interpreter.
5. System candidates: `python3`, `python`.
6. Windows only: `py -3` launcher.
7. Unavailable.

`KODA_DISABLE_PYTHON_DISCOVERY=1` should disable managed/workspace/system discovery in tests and diagnostics, leaving only explicit `KODA_PYTHON` / `PYTHON` values.

Interpreter paths:

| Platform | Venv Python | Venv pip |
| --- | --- | --- |
| macOS/Linux | `venv/bin/python` | `venv/bin/python -m pip` |
| Windows | `venv\Scripts\python.exe` | `venv\Scripts\python.exe -m pip` |

Do not call `pip` as a standalone binary; always use `<python> -m pip`.

Windows `py -3` is not a plain executable path. Represent candidates as command specs, not only `PathBuf`, so the resolver can store `program="py"` and `args=["-3"]` while normal venv/system candidates store an empty arg list.

Target Python 3.10+ to match upstream `pyproject.toml`, and prefer Python 3.12 for bootstrap. Phase 1 resolver may accept Python 3.8/3.9 as legacy execution fallback so existing systems do not regress before managed bootstrap exists, but doctor should warn that GenericAgent helper parity expects 3.10+.

Candidate validation must run a short probe, not just `--version`:

```text
<python> -c "import sys, json; print(json.dumps({'version': sys.version_info[:3], 'prefix': sys.prefix, 'base_prefix': sys.base_prefix}))"
```

For Windows launcher candidates, the probe becomes `py -3 -c ...`.

## Managed Directories

Default managed venv should be user-level, not repo-level:

| Platform | Managed venv | Cache |
| --- | --- | --- |
| macOS | `~/Library/Application Support/koda-agent/python/venv` | `~/Library/Caches/koda-agent` |
| Linux | `$XDG_DATA_HOME/koda-agent/python/venv` or `~/.local/share/koda-agent/python/venv` | `$XDG_CACHE_HOME/koda-agent` or `~/.cache/koda-agent` |
| Windows | `%LOCALAPPDATA%\koda-agent\python\venv` | `%LOCALAPPDATA%\koda-agent\cache` |

Implementation can use the existing `dirs` crate first; if later we want app-specific semantics, switch to `directories::ProjectDirs`.

## Rust API Shape

Add a shared module, preferably in `koda-agent-core` so tools, memory, CLI, and xtask can reuse it.

```rust
pub struct PythonRuntime {
    pub command: PythonCommand,
    pub source: PythonSource,
    pub version: Option<String>,
    pub venv_dir: Option<PathBuf>,
    pub is_venv: bool,
}

pub struct PythonCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

pub enum PythonSource {
    EnvKodaPython,
    EnvPython,
    ManagedVenv,
    WorkspaceVenv,
    WorkspaceKodaVenv,
    SystemPath,
    WindowsPyLauncher,
}

pub struct PythonDoctorReport {
    pub available: bool,
    pub runtime: Option<PythonRuntime>,
    pub capabilities: Vec<PythonCapabilityStatus>,
    pub recommendations: Vec<String>,
}

pub enum PythonExtra {
    Core,
    Ocr,
    Automation,
    Dev,
}

pub enum PythonPurpose {
    AgentHelper,
    UserCode,
}
```

Required helpers:

- `resolve_python(root: &Path, purpose: PythonPurpose) -> Option<PythonRuntime>`
- `doctor_python(root: &Path) -> PythonDoctorReport`
- `managed_venv_dir() -> Result<PathBuf>`
- `venv_python(venv: &Path) -> PathBuf`
- `create_or_update_venv(root: &Path, extras: &[PythonExtra], recreate: bool) -> Result<BootstrapReport>`

Command execution helpers should support a `PythonIsolation` mode:

- `UserCode`: preserve current user-code semantics and workspace cwd.
- `AgentHelper`: set `PYTHONNOUSERSITE=1`, clear `PYTHONPATH`/`PYTHONHOME` unless explicitly allowed, set cwd to the repo root or a safe temp dir, and use `-I` for import probes where compatible.
- `Bootstrap`: use controlled environment plus proxy/index variables; do not inherit unrelated Python env vars.

## Bootstrap Strategy

Bootstrap should prefer `uv` if available, then fall back to system Python.

### With uv

```text
uv python install 3.12
uv venv <managed-venv> --python 3.12
uv pip install --python <venv-python> -r requirements-python-core.txt
```

Add extras by installing their requirement files after core.

Respect standard proxy/index environment variables during installation (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `PIP_INDEX_URL`, `PIP_EXTRA_INDEX_URL`, `UV_INDEX_URL`) but redact their credentials in logs.
Support `--offline` later by failing fast unless the managed venv already satisfies requested extras; do not partially install in offline mode.

### Without uv, with Python

```text
<system-python> -m venv <managed-venv>
<venv-python> -m pip install -U pip
<venv-python> -m pip install -r requirements-python-core.txt
```

### Without uv and without Python

Do not silently download installers in the first implementation. `doctor` should provide platform-specific instructions:

- macOS: install `uv` or Python 3.12+ via Homebrew/Python.org.
- Linux: install `python3`, `python3-venv`, and optionally `uv` using the distro package manager.
- Windows: install `uv`, Python.org Python, or `winget install Python.Python.3.12`.

A later `bootstrap-python --install-uv` can be considered, but it should be explicit because it downloads an external runtime.

Bootstrap must use a lock file under the managed Python directory so two concurrent `bootstrap-python` runs cannot corrupt the same venv.
`--repair` should reinstall requested requirements into the existing managed venv without deleting it. `--recreate` should delete and rebuild only after path ownership checks pass.

## Requirement Files

Add these files at the repo root:

```text
requirements-python-core.txt
requirements-python-ocr.txt
requirements-python-automation.txt
requirements-python-dev.txt
```

Initial policy:

- `core`: light compatibility dependencies only; avoid heavy native packages; start empty if no package is strictly required.
- `ocr`: `pillow`, RapidOCR stack, and OCR-only helpers.
- `automation`: GUI/system automation packages where applicable.
- `dev`: test-only packages.

Keep OS-native binaries out of pip requirements. `adb`, `tesseract`, Chrome/Edge, and platform accessibility permissions are separate doctor checks.

Pin versions once a dependency is added. Prefer wheel-friendly packages and avoid packages that require local compilation unless they are behind an explicit extra.
When the dependency set stabilizes, add hash-locked installs (`--require-hashes` or uv lock support) for release builds. During active development, a pinned requirements file is acceptable.

## Doctor Output

Human output should be concise and actionable:

```text
Core
  workspace: ok
  env: found, keys redacted

Python
  selected: managed-venv
  executable: .../venv/bin/python
  version: 3.12.x
  pip: ok

Python helpers
  core: ok
  ocr: missing extras: run koda-agent bootstrap-python --extras ocr
  automation: adb missing; install Android platform-tools

Browser
  tmwebdriver bridge: connected
```

JSON output should include stable fields for tests:

```json
{
  "python": {
    "available": true,
    "source": "managed_venv",
    "executable": "...",
    "version": "3.12.x"
  },
  "capabilities": [
    {"name":"core", "status":"ok"},
    {"name":"ocr", "status":"missing_extra", "fix":"koda-agent bootstrap-python --extras ocr"}
  ]
}
```

Doctor probes must avoid importing from the current project directory for agent-helper checks. Use isolated probes or set cwd to a safe temp dir so local files named like dependencies cannot shadow real packages.

## Tool Behavior

### `code_run`

- If `type=python|py`, call the shared resolver.
- If unavailable, return a tool error with:
  - `status=error`
  - `code=python_unavailable`
  - `fix=koda-agent bootstrap-python or set KODA_PYTHON=/path/to/python`
- If available, use the selected interpreter and preserve current header/memory-path injection.
- Use `PythonPurpose::UserCode` so workspace `.venv` is preferred over managed helper venv when users run project code.
- Do not force `-I` or strip `PYTHONPATH` for user code; that would break legitimate project execution. Only normalize missing-interpreter errors.

### Python reflect scripts

- Reuse the same resolver.
- If unavailable, show the same actionable message, plus mention native JSON reflect rules.

### Memory OCR/Vision helpers

- Use shared resolver for Python image preparation/OCR fallback.
- Use `PythonPurpose::AgentHelper` so the managed helper venv is preferred over arbitrary workspace dependencies.
- Keep native Vision API path available without Python when possible.
- If OCR dependencies are missing, return `missing_extra=ocr`, not raw import errors.
- For compatibility scripts under `memory/`, explicitly add the memory directory to `sys.path` inside the helper wrapper rather than relying on ambient cwd or user `PYTHONPATH`.

### xtask smokes

- Remove hard dependency on `python3` for generating smoke images where practical.
- Prefer a tiny Rust-generated image, or skip image generation with a clear message if the optional dependency is unavailable.

## Platform Notes

### Windows

- Do not run `activate.bat` or PowerShell activation.
- Prefer direct `venv\Scripts\python.exe` calls.
- Check `py -3` after `python` candidates.
- Paths may contain spaces; all process launching must use `Command.arg`.
- Store managed venv under `%LOCALAPPDATA%`.
- OCR/Tesseract and ADB are separate binary checks.

### Linux

- Debian/Ubuntu may require `python3-venv`.
- PEP 668 is fine because packages install inside venv.
- Native OCR may need distro packages such as `tesseract-ocr`, `libgl1`, or `libglib2.0-0`; doctor should only report these, not guess-install.
- Respect `XDG_DATA_HOME` and `XDG_CACHE_HOME`.

### macOS

- `/usr/bin/python3` can be old or incomplete; managed venv should prefer uv/Python 3.12+ when possible.
- Homebrew can live under `/opt/homebrew` or `/usr/local`.
- GUI automation may require Accessibility permission independent of Python availability.
- Keychain functionality can be Rust-native through `security`; Python keychain helper remains compatibility-only.

## Implementation Phases

### Phase 1: Resolver and Doctor

- Add shared Python runtime resolver in `koda-agent-core`.
- Replace duplicated `find_python_interpreter` / `python_candidates` call sites.
- Add `koda-agent doctor` and `koda-agent doctor --json`.
- Add unit tests for resolver path construction and env priority.
- Add tests for purpose-specific precedence and `KODA_DISABLE_PYTHON_DISCOVERY=1`.
- Add tests that probes do not import from cwd and that `py -3` command specs preserve args.

Acceptance:

- Core CLI starts with no Python.
- `doctor --json` reports Python missing without failing.
- `code_run type=python` returns structured missing-Python error.

### Phase 2: Bootstrap Managed Venv

- Add `bootstrap-python` CLI command.
- Add requirement files.
- Implement uv-first and venv fallback paths.
- Add `--extras`, `--recreate`, and dry report of installed files.
- Add managed-venv lock file and safe deletion that refuses to remove any path outside the managed directory.
- Add `--repair` and managed-env remove path checks.

Acceptance:

- macOS/Linux/Windows path logic is covered by unit tests.
- Bootstrap never uses global pip.
- Re-running bootstrap is idempotent.
- Concurrent bootstrap attempts serialize or fail cleanly.
- Recreate/remove cannot target workspace `.venv` or `KODA_PYTHON`.

Current implementation status: the first `bootstrap-python` command exists with managed venv creation, uv-first / `python -m venv` fallback, empty requirements skipping, `--extras`, `--recreate`, `--repair`, `--dry-run`, `--offline`, and lock-file protection. `doctor --json` includes managed venv status. `python-env remove` removes only the managed venv after path ownership checks. `KODA_BOOTSTRAP_DISABLE_UV=1` can force the no-network `python -m venv` path for tests. Stronger no-network tests remain follow-up hardening.

### Phase 3: Capability Checks

- Add capability probes for core Python, OCR, Vision helper, ADB, Tesseract, GUI automation prerequisites.
- Convert import failures into stable capability errors.
- Update README with install/doctor flow.

Acceptance:

- Missing OCR packages produce `bootstrap-python --extras ocr` guidance.
- Missing ADB produces platform-tools guidance.
- Existing native Vision path still works without Python.

### Phase 4: No-Python and Full-Parity Gates

Add validation targets:

```bash
cargo test --workspace --all-features
cargo run -q -p koda-agent-cli -- doctor --json
KODA_DISABLE_PYTHON_DISCOVERY=1 cargo test -p koda-agent-tools python_unavailable
cargo run -q -p koda-agent-cli -- bootstrap-python --extras core
cargo run -q -p xtask -- memory-parity-smoke
```

Acceptance:

- No-Python core gate passes.
- Full Python helper gate passes on a machine with managed venv.
- No secrets are printed in doctor/bootstrap logs.

## Risks and Mitigations

| Risk | Mitigation |
| --- | --- |
| `uv` not installed | Fall back to system Python venv; doctor gives explicit install instructions if neither exists. |
| Windows Store Python alias is broken | Verify `--version` and a short `-c` command before accepting candidate. |
| Heavy OCR dependencies fail to build | Keep OCR as optional extra; prefer wheels; report native system packages separately. |
| User wants project-specific Python | Support `KODA_PYTHON` and workspace `.venv` before system fallback. |
| LLM sees raw import errors | Normalize missing dependency errors before returning tool output. |
| Managed venv grows stale | `bootstrap-python --recreate` removes/rebuilds only managed venv, never user venvs. |
| User is behind proxy or mirror | Respect standard proxy and package-index env vars; redact credentials in logs. |
| Project `.venv` conflicts with helper deps | Use purpose-specific resolver order so agent helpers prefer managed venv and user code prefers workspace venv. |
| Import probe is shadowed by files in cwd | Run doctor/helper import probes in isolated mode or safe cwd; clear Python path env for agent-helper probes. |
| Supply-chain drift in Python packages | Pin requirements now; add hash locks for release builds once dependency set stabilizes. |
| User needs cleanup | Provide `python-env remove` with strict managed-path ownership checks. |

## Self Review

- Cross-platform: covered macOS/Linux/Windows path conventions, launch semantics, and package manager differences.
- Safety: no global pip, no implicit downloads during normal execution, no shell activation, no secret printing.
- Compatibility: preserves upstream Python helper files and SOP imports while making Rust core independent.
- Maintainability: central resolver removes duplicated logic across CLI/tools/memory/xtask.
- Testability: includes JSON doctor output and no-Python/full-parity gates.
- Second-pass review: added purpose-specific resolution, Windows `py -3` command specs, discovery-disable testing, proxy/index handling, dependency pinning, and bootstrap locking/safe deletion.
- Third-pass review: added Python version policy, validated probes, execution isolation, cwd shadowing protection, repair/remove commands, offline behavior, and future hash-locked install guidance.
- Remaining decision: whether to add an explicit `--install-uv` path later. This is intentionally deferred because it downloads external runtime components.
