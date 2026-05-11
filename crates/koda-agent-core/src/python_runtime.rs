use crate::default_koda_home;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PythonPurpose {
    AgentHelper,
    UserCode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PythonCommand {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
}

impl PythonCommand {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    pub fn with_args(program: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }

    pub fn std_command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        cmd
    }

    pub fn display(&self) -> String {
        let mut out = self.program.display().to_string();
        for arg in &self.args {
            out.push(' ');
            out.push_str(arg);
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PythonSource {
    EnvKodaPython,
    EnvPython,
    ManagedVenv,
    LegacyManagedVenv,
    WorkspaceVenv,
    WorkspaceKodaVenv,
    SystemPath,
    WindowsPyLauncher,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PythonRuntime {
    pub command: PythonCommand,
    pub source: PythonSource,
    pub version: Option<String>,
    pub venv_dir: Option<PathBuf>,
    pub is_venv: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PythonDoctorReport {
    pub available: bool,
    pub runtime: Option<PythonRuntime>,
    pub checked: Vec<PythonCandidateReport>,
    pub managed_venv: Option<PythonManagedVenvStatus>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PythonCandidateReport {
    pub source: PythonSource,
    pub command: String,
    pub ok: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PythonManagedVenvStatus {
    pub venv_dir: PathBuf,
    pub exists: bool,
    pub python: PathBuf,
    pub python_exists: bool,
    pub available: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct PythonProbe {
    version: String,
    is_venv: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PythonExtra {
    Core,
    Ocr,
    Automation,
    Dev,
}

impl PythonExtra {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "core" => Some(Self::Core),
            "ocr" => Some(Self::Ocr),
            "automation" | "auto" => Some(Self::Automation),
            "dev" => Some(Self::Dev),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PythonBootstrapReport {
    pub venv_dir: PathBuf,
    pub python: PathBuf,
    pub created: bool,
    pub repaired: bool,
    pub dry_run: bool,
    pub offline: bool,
    pub installer: String,
    pub extras: Vec<PythonExtra>,
    pub installed_requirements: Vec<PathBuf>,
    pub skipped_empty_requirements: Vec<PathBuf>,
    pub planned_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PythonEnvRemoveReport {
    pub venv_dir: PathBuf,
    pub removed: bool,
}

pub fn resolve_python(root: &Path, purpose: PythonPurpose) -> Option<PythonRuntime> {
    python_candidates(root, purpose)
        .into_iter()
        .find_map(|(source, command, venv_dir)| {
            probe_python(&command).ok().map(|probe| PythonRuntime {
                command,
                source,
                version: Some(probe.version),
                venv_dir,
                is_venv: probe.is_venv,
            })
        })
}

pub fn doctor_python(root: &Path, purpose: PythonPurpose) -> PythonDoctorReport {
    let mut checked = Vec::new();
    let mut runtime = None;
    for (source, command, venv_dir) in python_candidates(root, purpose) {
        let command_label = command.display();
        match probe_python(&command) {
            Ok(probe) => {
                let report = PythonCandidateReport {
                    source: source.clone(),
                    command: command_label,
                    ok: true,
                    version: Some(probe.version.clone()),
                    error: None,
                };
                if runtime.is_none() {
                    runtime = Some(PythonRuntime {
                        command,
                        source,
                        version: Some(probe.version),
                        venv_dir,
                        is_venv: probe.is_venv,
                    });
                }
                checked.push(report);
            }
            Err(error) => checked.push(PythonCandidateReport {
                source,
                command: command_label,
                ok: false,
                version: None,
                error: Some(error),
            }),
        }
    }
    let mut recommendations = Vec::new();
    if let Some(runtime) = &runtime {
        if runtime
            .version
            .as_deref()
            .is_some_and(|v| python_version_lt(v, 3, 10))
        {
            recommendations.push(
                "Python is available but older than GenericAgent's preferred 3.10+; bootstrap should use Python 3.12 when implemented.".to_string(),
            );
        }
    } else {
        recommendations.push(
            "Python unavailable. Install Python 3.10+ (3.12 preferred), run future `koda-agent bootstrap-python`, or set KODA_PYTHON=/path/to/python.".to_string(),
        );
    }
    PythonDoctorReport {
        available: runtime.is_some(),
        runtime,
        checked,
        managed_venv: managed_venv_status(),
        recommendations,
    }
}

pub fn managed_python_venv_dir() -> Option<PathBuf> {
    if let Ok(path) = env::var("KODA_MANAGED_PYTHON_DIR")
        && !path.trim().is_empty()
    {
        return Some(PathBuf::from(path));
    }
    Some(default_koda_home().join("python").join("venv"))
}

fn legacy_managed_python_venv_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        dirs::data_local_dir().map(|p| p.join("koda-agent").join("python").join("venv"))
    }
    #[cfg(target_os = "macos")]
    {
        dirs::data_dir().map(|p| p.join("koda-agent").join("python").join("venv"))
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        dirs::data_dir().map(|p| p.join("koda-agent").join("python").join("venv"))
    }
}

pub fn venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

pub fn python_unavailable_message() -> &'static str {
    "Python runtime is unavailable. Run `koda-agent bootstrap-python`, set KODA_PYTHON=/path/to/python, or use a non-Python tool mode."
}

pub fn bootstrap_managed_python(
    root: &Path,
    extras: &[PythonExtra],
    recreate: bool,
    repair: bool,
    dry_run: bool,
    offline: bool,
) -> Result<PythonBootstrapReport> {
    if recreate && repair {
        bail!("--recreate and --repair are mutually exclusive");
    }
    let venv_dir = managed_python_venv_dir().context("managed Python directory unavailable")?;
    let python = venv_python(&venv_dir);
    let parent = venv_dir
        .parent()
        .context("managed Python venv has no parent")?
        .to_path_buf();
    let extras = normalize_extras(extras);
    let mut installed_requirements = Vec::new();
    let mut skipped_empty_requirements = Vec::new();
    let mut planned_actions = Vec::new();
    if dry_run {
        if recreate && venv_dir.exists() {
            planned_actions.push(format!("remove managed venv {}", venv_dir.display()));
        }
        if !python.exists() {
            planned_actions.push(format!("create managed venv {}", venv_dir.display()));
        }
        if repair {
            planned_actions.push(format!("repair managed venv {}", venv_dir.display()));
        }
        for extra in &extras {
            let req = requirements_path(root, *extra);
            if !req.exists() || requirements_is_empty(&req)? {
                skipped_empty_requirements.push(req);
            } else {
                planned_actions.push(format!("install requirements {}", req.display()));
                installed_requirements.push(req);
            }
        }
        return Ok(PythonBootstrapReport {
            venv_dir,
            python,
            created: false,
            repaired: false,
            dry_run,
            offline,
            installer: "dry_run".into(),
            extras,
            installed_requirements,
            skipped_empty_requirements,
            planned_actions,
        });
    }
    fs::create_dir_all(&parent)?;
    let _lock = BootstrapLock::acquire(&parent.join(".bootstrap.lock"))?;
    if recreate && venv_dir.exists() {
        ensure_managed_venv_path(&venv_dir)?;
        fs::remove_dir_all(&venv_dir)
            .with_context(|| format!("remove managed venv {}", venv_dir.display()))?;
    }
    let mut created = false;
    let mut repaired = false;
    let mut installer = String::from("existing");
    if !python.exists() {
        if offline {
            bail!(
                "offline bootstrap cannot create missing managed venv {}",
                venv_dir.display()
            );
        }
        if !env_truthy("KODA_BOOTSTRAP_DISABLE_UV")
            && let Some(uv) = find_program("uv")
        {
            run_command(
                Command::new(&uv)
                    .arg("venv")
                    .arg(&venv_dir)
                    .arg("--python")
                    .arg("3.12"),
                "create managed Python venv with uv",
            )?;
            installer = "uv".into();
        } else {
            let seed = resolve_seed_python(root).context(
                "no Python seed found for venv creation; install Python 3.10+ or uv first",
            )?;
            run_command(
                seed.command
                    .std_command()
                    .arg("-m")
                    .arg("venv")
                    .arg(&venv_dir),
                "create managed Python venv with python -m venv",
            )?;
            installer = seed.command.display();
        }
        created = true;
    }
    if !python.exists() {
        bail!(
            "managed Python venv was created but {} is missing",
            python.display()
        );
    }
    if repair {
        let _ = run_command(
            Command::new(&python)
                .arg("-m")
                .arg("ensurepip")
                .arg("--upgrade"),
            "repair managed Python ensurepip",
        );
        repaired = true;
        if installer == "existing" {
            installer = "repair".into();
        }
    }
    for extra in &extras {
        let req = requirements_path(root, *extra);
        if !req.exists() || requirements_is_empty(&req)? {
            skipped_empty_requirements.push(req);
            continue;
        }
        if offline {
            bail!(
                "offline bootstrap cannot install non-empty requirements {}",
                req.display()
            );
        }
        run_command(
            Command::new(&python)
                .arg("-m")
                .arg("pip")
                .arg("install")
                .arg("-r")
                .arg(&req),
            "install managed Python requirements",
        )?;
        installed_requirements.push(req);
    }
    Ok(PythonBootstrapReport {
        venv_dir,
        python,
        created,
        repaired,
        dry_run,
        offline,
        installer,
        extras,
        installed_requirements,
        skipped_empty_requirements,
        planned_actions,
    })
}

pub fn remove_managed_python() -> Result<PythonEnvRemoveReport> {
    let venv_dir = managed_python_venv_dir().context("managed Python directory unavailable")?;
    ensure_managed_venv_path(&venv_dir)?;
    let parent = venv_dir
        .parent()
        .context("managed Python venv has no parent")?
        .to_path_buf();
    fs::create_dir_all(&parent)?;
    let _lock = BootstrapLock::acquire(&parent.join(".bootstrap.lock"))?;
    let removed = if venv_dir.exists() {
        fs::remove_dir_all(&venv_dir)
            .with_context(|| format!("remove managed venv {}", venv_dir.display()))?;
        true
    } else {
        false
    };
    Ok(PythonEnvRemoveReport { venv_dir, removed })
}

fn managed_venv_status() -> Option<PythonManagedVenvStatus> {
    let venv_dir = managed_python_venv_dir()?;
    let python = venv_python(&venv_dir);
    let exists = venv_dir.exists();
    let python_exists = python.exists();
    let command = PythonCommand::new(python.clone());
    let (available, version, error) = match probe_python(&command) {
        Ok(probe) => (true, Some(probe.version), None),
        Err(err) => (false, None, Some(err)),
    };
    Some(PythonManagedVenvStatus {
        venv_dir,
        exists,
        python,
        python_exists,
        available,
        version,
        error,
    })
}

fn python_candidates(
    root: &Path,
    purpose: PythonPurpose,
) -> Vec<(PythonSource, PythonCommand, Option<PathBuf>)> {
    let mut out = Vec::new();
    push_env_candidate(&mut out, "KODA_PYTHON", PythonSource::EnvKodaPython);
    if matches!(purpose, PythonPurpose::AgentHelper) {
        push_env_candidate(&mut out, "PYTHON", PythonSource::EnvPython);
    }
    let discovery_disabled = env_truthy("KODA_DISABLE_PYTHON_DISCOVERY");
    if !discovery_disabled {
        match purpose {
            PythonPurpose::AgentHelper => {
                push_managed(&mut out);
                push_venv(
                    &mut out,
                    root.join(".koda/python/venv"),
                    PythonSource::WorkspaceKodaVenv,
                );
                push_venv(&mut out, root.join(".venv"), PythonSource::WorkspaceVenv);
            }
            PythonPurpose::UserCode => {
                push_venv(&mut out, root.join(".venv"), PythonSource::WorkspaceVenv);
                push_venv(
                    &mut out,
                    root.join(".koda/python/venv"),
                    PythonSource::WorkspaceKodaVenv,
                );
                push_managed(&mut out);
            }
        }
        push_system(&mut out, "python3");
        push_system(&mut out, "python");
        #[cfg(windows)]
        out.push((
            PythonSource::WindowsPyLauncher,
            PythonCommand::with_args("py", vec!["-3".to_string()]),
            None,
        ));
    }
    dedupe_candidates(out)
}

fn push_env_candidate(
    out: &mut Vec<(PythonSource, PythonCommand, Option<PathBuf>)>,
    key: &str,
    source: PythonSource,
) {
    if let Ok(value) = env::var(key) {
        let value = value.trim();
        if !value.is_empty() {
            out.push((source, PythonCommand::new(value), None));
        }
    }
}

fn push_managed(out: &mut Vec<(PythonSource, PythonCommand, Option<PathBuf>)>) {
    if let Some(venv) = managed_python_venv_dir() {
        push_venv(out, venv, PythonSource::ManagedVenv);
    }
    if let Some(venv) = legacy_managed_python_venv_dir() {
        push_venv(out, venv, PythonSource::LegacyManagedVenv);
    }
}

fn push_venv(
    out: &mut Vec<(PythonSource, PythonCommand, Option<PathBuf>)>,
    venv: PathBuf,
    source: PythonSource,
) {
    let python = venv_python(&venv);
    out.push((source, PythonCommand::new(python), Some(venv)));
}

fn push_system(out: &mut Vec<(PythonSource, PythonCommand, Option<PathBuf>)>, program: &str) {
    out.push((PythonSource::SystemPath, PythonCommand::new(program), None));
}

fn dedupe_candidates(
    candidates: Vec<(PythonSource, PythonCommand, Option<PathBuf>)>,
) -> Vec<(PythonSource, PythonCommand, Option<PathBuf>)> {
    let mut out = Vec::new();
    let mut seen = Vec::<String>::new();
    for candidate in candidates {
        let key = candidate.1.display();
        if !seen.iter().any(|v| v == &key) {
            seen.push(key);
            out.push(candidate);
        }
    }
    out
}

fn probe_python(command: &PythonCommand) -> Result<PythonProbe, String> {
    let script = r#"import json, sys
print(json.dumps({'version': list(sys.version_info[:3]), 'prefix': sys.prefix, 'base_prefix': getattr(sys, 'base_prefix', sys.prefix)}))"#;
    let mut cmd = command.std_command();
    cmd.arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped());
    let output = cmd.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("invalid probe JSON: {e}"))?;
    let nums = value
        .get("version")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "probe JSON missing version".to_string())?;
    let major = nums
        .first()
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let minor = nums
        .get(1)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let patch = nums
        .get(2)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    if major < 3 || (major == 3 && minor < 8) {
        return Err(format!(
            "Python {major}.{minor}.{patch} is too old; need 3.8+ for legacy execution"
        ));
    }
    let prefix = value
        .get("prefix")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let base_prefix = value
        .get("base_prefix")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(prefix);
    Ok(PythonProbe {
        version: format!("{major}.{minor}.{patch}"),
        is_venv: prefix != base_prefix,
    })
}

fn python_version_lt(version: &str, want_major: u64, want_minor: u64) -> bool {
    let mut parts = version.split('.').filter_map(|p| p.parse::<u64>().ok());
    let major = parts.next().unwrap_or_default();
    let minor = parts.next().unwrap_or_default();
    major < want_major || (major == want_major && minor < want_minor)
}

fn env_truthy(key: &str) -> bool {
    env::var(key)
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn resolve_seed_python(root: &Path) -> Option<PythonRuntime> {
    let mut candidates = Vec::new();
    push_env_candidate(&mut candidates, "KODA_PYTHON", PythonSource::EnvKodaPython);
    push_system(&mut candidates, "python3");
    push_system(&mut candidates, "python");
    #[cfg(windows)]
    candidates.push((
        PythonSource::WindowsPyLauncher,
        PythonCommand::with_args("py", vec!["-3".to_string()]),
        None,
    ));
    candidates
        .into_iter()
        .chain(python_candidates(root, PythonPurpose::UserCode))
        .find_map(|(source, command, venv_dir)| {
            probe_python(&command).ok().map(|probe| PythonRuntime {
                command,
                source,
                version: Some(probe.version),
                venv_dir,
                is_venv: probe.is_venv,
            })
        })
}

fn find_program(program: &str) -> Option<PathBuf> {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success())
        .map(|_| PathBuf::from(program))
}

fn normalize_extras(extras: &[PythonExtra]) -> Vec<PythonExtra> {
    let mut out = vec![PythonExtra::Core];
    for extra in extras {
        if matches!(extra, PythonExtra::Core) {
            continue;
        }
        if !out.iter().any(|seen| seen == extra) {
            out.push(*extra);
        }
    }
    out
}

fn requirements_path(root: &Path, extra: PythonExtra) -> PathBuf {
    let name = match extra {
        PythonExtra::Core => "requirements-python-core.txt",
        PythonExtra::Ocr => "requirements-python-ocr.txt",
        PythonExtra::Automation => "requirements-python-automation.txt",
        PythonExtra::Dev => "requirements-python-dev.txt",
    };
    root.join(name)
}

fn requirements_is_empty(path: &Path) -> Result<bool> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .all(|line| line.is_empty() || line.starts_with('#')))
}

fn ensure_managed_venv_path(venv_dir: &Path) -> Result<()> {
    let managed = managed_python_venv_dir().context("managed Python directory unavailable")?;
    let managed = normalize_path_for_compare(&managed)?;
    let actual = normalize_path_for_compare(venv_dir)?;
    if actual != managed {
        bail!(
            "refusing to modify non-managed Python venv: {}",
            venv_dir.display()
        );
    }
    Ok(())
}

fn normalize_path_for_compare(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        Ok(path.canonicalize()?)
    } else {
        let parent = path
            .parent()
            .context("path has no parent")?
            .canonicalize()
            .with_context(|| format!("canonicalize parent of {}", path.display()))?;
        Ok(parent.join(path.file_name().unwrap_or_default()))
    }
}

fn run_command(cmd: &mut Command, label: &str) -> Result<()> {
    let output = cmd.output().with_context(|| label.to_string())?;
    if !output.status.success() {
        bail!(
            "{label} failed with {}: {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

struct BootstrapLock {
    path: PathBuf,
}

impl BootstrapLock {
    fn acquire(path: &Path) -> Result<Self> {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(_) => Ok(Self {
                path: path.to_path_buf(),
            }),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                bail!("another bootstrap-python process is already running")
            }
            Err(err) => Err(err).with_context(|| format!("create lock {}", path.display())),
        }
    }
}

impl Drop for BootstrapLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn venv_python_is_platform_specific() {
        let path = venv_python(Path::new("/tmp/koda-venv"));
        if cfg!(windows) {
            assert!(path.ends_with(Path::new("Scripts/python.exe")));
        } else {
            assert!(path.ends_with(Path::new("bin/python")));
        }
    }

    #[test]
    fn python_command_display_preserves_args() {
        let cmd = PythonCommand::with_args("py", vec!["-3".into()]);
        assert_eq!(cmd.display(), "py -3");
    }

    #[test]
    fn managed_python_defaults_to_koda_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        unsafe {
            std::env::remove_var("KODA_MANAGED_PYTHON_DIR");
            std::env::set_var("KODA_AGENT_HOME", d.path());
        }
        let managed = managed_python_venv_dir().unwrap();
        unsafe { std::env::remove_var("KODA_AGENT_HOME") };
        assert_eq!(managed, d.path().join("python/venv"));
    }

    #[test]
    fn candidate_order_is_purpose_specific() {
        let root = Path::new("/tmp/koda-python-order");
        let helper_sources = python_candidates(root, PythonPurpose::AgentHelper)
            .into_iter()
            .map(|(source, _, _)| source)
            .collect::<Vec<_>>();
        let user_sources = python_candidates(root, PythonPurpose::UserCode)
            .into_iter()
            .map(|(source, _, _)| source)
            .collect::<Vec<_>>();
        let helper_managed = helper_sources
            .iter()
            .position(|s| s == &PythonSource::ManagedVenv)
            .unwrap();
        let helper_workspace = helper_sources
            .iter()
            .position(|s| s == &PythonSource::WorkspaceVenv)
            .unwrap();
        let user_managed = user_sources
            .iter()
            .position(|s| s == &PythonSource::ManagedVenv)
            .unwrap();
        let user_workspace = user_sources
            .iter()
            .position(|s| s == &PythonSource::WorkspaceVenv)
            .unwrap();
        assert!(helper_managed < helper_workspace);
        assert!(user_workspace < user_managed);
    }

    #[test]
    fn extras_are_core_first_and_deduped() {
        assert_eq!(normalize_extras(&[]), vec![PythonExtra::Core]);
        assert_eq!(
            normalize_extras(&[
                PythonExtra::Ocr,
                PythonExtra::Core,
                PythonExtra::Ocr,
                PythonExtra::Automation
            ]),
            vec![PythonExtra::Core, PythonExtra::Ocr, PythonExtra::Automation]
        );
    }

    #[test]
    fn empty_requirement_files_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let req = d.path().join("requirements.txt");
        std::fs::write(&req, "\n# comment only\n\n").unwrap();
        assert!(requirements_is_empty(&req).unwrap());
        std::fs::write(&req, "requests==2.32.0\n").unwrap();
        assert!(!requirements_is_empty(&req).unwrap());
    }

    #[test]
    fn bootstrap_dry_run_plans_without_creating_venv() {
        let _guard = ENV_LOCK.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        let managed = d.path().join("managed/venv");
        std::fs::write(
            d.path().join("requirements-python-core.txt"),
            "requests==2.32.0\n",
        )
        .unwrap();
        // Rust 2024 marks process environment mutation unsafe; serialize this test.
        unsafe { std::env::set_var("KODA_MANAGED_PYTHON_DIR", &managed) };
        let report =
            bootstrap_managed_python(d.path(), &[PythonExtra::Core], false, false, true, false)
                .unwrap();
        unsafe { std::env::remove_var("KODA_MANAGED_PYTHON_DIR") };

        assert!(report.dry_run);
        assert!(!report.created);
        assert_eq!(report.installer, "dry_run");
        assert!(!managed.exists());
        assert!(
            report
                .planned_actions
                .iter()
                .any(|a| a.contains("create managed venv"))
        );
        assert!(
            report
                .planned_actions
                .iter()
                .any(|a| a.contains("install requirements"))
        );
    }

    #[test]
    fn bootstrap_offline_fails_before_network_when_venv_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        let managed = d.path().join("managed/venv");
        unsafe { std::env::set_var("KODA_MANAGED_PYTHON_DIR", &managed) };
        let err =
            bootstrap_managed_python(d.path(), &[PythonExtra::Core], false, false, false, true)
                .unwrap_err();
        unsafe { std::env::remove_var("KODA_MANAGED_PYTHON_DIR") };

        assert!(
            format!("{err:#}").contains("offline bootstrap cannot create missing managed venv")
        );
        assert!(!managed.exists());
    }
}
