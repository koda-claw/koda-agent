mod tui_full;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDateTime, Timelike};
use clap::{Parser, Subcommand};
use crossterm::{
    execute,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{Clear, ClearType},
};
use koda_agent_core::{
    AgentConfig, AgentEvent, AgentPathOptions, AgentRuntime,
    python_runtime::{
        PythonExtra, PythonPurpose, bootstrap_managed_python, doctor_python,
        python_unavailable_message, remove_managed_python, resolve_python,
    },
    resolve_agent_paths_with_options,
};
use koda_agent_frontends::{run_frontend, serve_acp_jsonl_with_factory};
use koda_agent_llm::OpenAiClient;
use koda_agent_memory::{
    archive_l4_sessions, audit_memory, cleanup_memory_indexes, init_memory, recall_l4_history,
    settle_long_term_updates, settle_long_term_updates_assisted,
};
use koda_agent_tools::GenericToolDispatcher;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::time::{sleep, timeout};

const DEFAULT_RELEASE_REPO: &str = "koda-claw/koda-agent";

#[derive(Parser, Debug)]
#[command(
    name = "koda-agent",
    version,
    about = "Rust GenericAgent-compatible runtime"
)]
struct Args {
    #[arg(long, help = "Override Koda home directory; defaults to ~/.koda-agent")]
    home: Option<PathBuf>,
    #[arg(
        long,
        help = "Override workspace directory for file tools; defaults to cwd"
    )]
    workspace: Option<PathBuf>,
    #[arg(
        long = "resource-dir",
        help = "Override packaged/source resource directory"
    )]
    resource_dir: Option<PathBuf>,
    #[arg(long, help = "One-shot task directory name under temp/")]
    task: Option<String>,
    #[arg(long, help = "Prompt text for one-shot execution")]
    input: Option<String>,
    #[arg(
        long = "reflect",
        alias = "reflect-rule",
        help = "Reflect script/rule: poll Python check() or native JSON rule and feed triggered tasks"
    )]
    reflect_rule: Option<String>,
    #[arg(long)]
    llm_no: Option<usize>,
    #[arg(long)]
    verbose: bool,
    #[arg(
        long,
        hide = true,
        help = "Run --task in foreground; used by background launcher"
    )]
    nobg: bool,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand, Debug)]
enum CliCommand {
    Doctor {
        #[arg(long)]
        json: bool,
    },
    BootstrapPython {
        #[arg(
            long,
            value_delimiter = ',',
            help = "Extras: core,ocr,automation,dev,all"
        )]
        extras: Vec<String>,
        #[arg(long, help = "Recreate only the managed Koda Python venv")]
        recreate: bool,
        #[arg(long, help = "Repair the managed venv without deleting it")]
        repair: bool,
        #[arg(
            long,
            help = "Show planned managed Python actions without changing files"
        )]
        dry_run: bool,
        #[arg(
            long,
            help = "Do not create venvs or install packages from the network"
        )]
        offline: bool,
    },
    PythonEnv {
        #[command(subcommand)]
        command: PythonEnvCommand,
    },
    Resources {
        #[command(subcommand)]
        command: ResourceCommand,
    },
    Update {
        #[arg(
            long,
            help = "GitHub repository OWNER/REPO; defaults to KODA_AGENT_REPO or koda-claw/koda-agent"
        )]
        repo: Option<String>,
        #[arg(
            long,
            default_value = "latest",
            help = "Release tag to install, or latest"
        )]
        version: String,
        #[arg(
            long,
            help = "Install prefix; defaults to the current executable prefix"
        )]
        prefix: Option<PathBuf>,
        #[arg(
            long,
            help = "Show planned update actions without downloading or changing files"
        )]
        dry_run: bool,
        #[arg(long, help = "Check GitHub latest release without installing")]
        check: bool,
        #[arg(long, help = "Emit update check result as JSON")]
        json: bool,
        #[arg(long, help = "Skip resource repair after the binary update")]
        no_resources: bool,
        #[arg(
            long,
            help = "Create/repair the managed helper Python environment after update"
        )]
        bootstrap_python: bool,
    },
    Tui {
        #[arg(long, hide = true, conflicts_with = "line")]
        full: bool,
        #[arg(long, conflicts_with = "full", help = "Use the stable line-mode TUI")]
        line: bool,
    },
    ServeAcp,
    Frontend {
        name: String,
    },
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
}

#[derive(Subcommand, Debug)]
enum PythonEnvCommand {
    Remove,
}

#[derive(Subcommand, Debug)]
enum ResourceCommand {
    Install {
        #[arg(long, help = "Resource source root; defaults to resolved resource_dir")]
        source: Option<PathBuf>,
        #[arg(long, help = "Overwrite existing resource files")]
        repair: bool,
        #[arg(long, help = "Show planned resource copies without changing files")]
        dry_run: bool,
    },
    Doctor {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum MemoryCommand {
    Settle {
        #[arg(
            long,
            help = "Use configured LLM to classify unsupported memory notes into safe patches"
        )]
        assisted: bool,
    },
    L4Archive {
        #[arg(long, help = "Execute archive; default is dry run")]
        run: bool,
        #[arg(long, help = "Override raw model_responses source directory")]
        src: Option<String>,
    },
    Audit,
    Cleanup {
        #[arg(long, help = "Execute cleanup; default is dry run")]
        run: bool,
        #[arg(long, help = "Also add missing L1 pointers for existing L2/L3 entries")]
        sync_missing: bool,
    },
    Recall {
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("warn").init();
    let args = Args::parse();
    let root = std::env::current_dir()?;
    let path_options = AgentPathOptions {
        home_dir: args.home.clone(),
        workspace_dir: args.workspace.clone(),
        resource_dir: args.resource_dir.clone(),
        executable_dir: current_exe_dir(),
    };
    if let Some(CliCommand::Doctor { json }) = &args.command {
        return run_doctor(&root, path_options, *json);
    }
    if let Some(CliCommand::BootstrapPython {
        extras,
        recreate,
        repair,
        dry_run,
        offline,
    }) = &args.command
    {
        let paths = resolve_agent_paths_with_options(&root, path_options);
        return run_bootstrap_python(
            &paths.resource_dir,
            extras,
            *recreate,
            *repair,
            *dry_run,
            *offline,
        );
    }
    if let Some(CliCommand::PythonEnv { command }) = &args.command {
        return run_python_env(command);
    }
    if let Some(CliCommand::Resources { command }) = &args.command {
        return run_resources(&root, path_options, command);
    }
    if let Some(CliCommand::Update {
        repo,
        version,
        prefix,
        dry_run,
        check,
        json,
        no_resources,
        bootstrap_python,
    }) = &args.command
    {
        return run_update(
            &root,
            path_options,
            UpdateRequest {
                repo: repo.as_deref(),
                version,
                prefix: prefix.as_deref(),
                dry_run: *dry_run,
                check: *check,
                json: *json,
                repair_resources: !*no_resources,
                bootstrap_python: *bootstrap_python,
            },
        )
        .await;
    }
    let mut cfg = AgentConfig::from_env_with_path_options(root, path_options)?;
    if args.verbose {
        cfg.verbose = true;
    }
    init_memory(&cfg)?;

    match args.command {
        Some(CliCommand::ServeAcp) => {
            let cfg_for_factory = cfg.clone();
            let factory = Arc::new(move || build_runtime(cfg_for_factory.clone()));
            return serve_acp_jsonl_with_factory(factory).await;
        }
        Some(CliCommand::Frontend { name }) => {
            return run_frontend(&name, build_runtime(cfg)?).await;
        }
        Some(CliCommand::Doctor { .. }) => unreachable!("doctor handled before config load"),
        Some(CliCommand::BootstrapPython { .. }) => {
            unreachable!("bootstrap-python handled before config load")
        }
        Some(CliCommand::PythonEnv { .. }) => unreachable!("python-env handled before config load"),
        Some(CliCommand::Resources { .. }) => unreachable!("resources handled before config load"),
        Some(CliCommand::Update { .. }) => unreachable!("update handled before config load"),
        Some(CliCommand::Memory {
            command: MemoryCommand::Settle { assisted },
        }) => {
            let report = if assisted {
                let llm = OpenAiClient::multi_arc(cfg.clone());
                settle_long_term_updates_assisted(&cfg, llm.as_ref()).await?
            } else {
                settle_long_term_updates(&cfg)?
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
            return Ok(());
        }
        Some(CliCommand::Memory {
            command: MemoryCommand::L4Archive { run, src },
        }) => {
            let src_path = src.as_deref().map(Path::new);
            let report = archive_l4_sessions(&cfg, src_path, !run)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            return Ok(());
        }
        Some(CliCommand::Memory {
            command: MemoryCommand::Audit,
        }) => {
            let report = audit_memory(&cfg)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            return Ok(());
        }
        Some(CliCommand::Memory {
            command: MemoryCommand::Cleanup { run, sync_missing },
        }) => {
            let report = cleanup_memory_indexes(&cfg, !run, sync_missing)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            return Ok(());
        }
        Some(CliCommand::Memory {
            command: MemoryCommand::Recall { query, limit },
        }) => {
            let hits = recall_l4_history(&cfg, &query, limit)?;
            println!("{}", serde_json::to_string_pretty(&hits)?);
            return Ok(());
        }
        Some(CliCommand::Tui { full, line }) => {
            if full || (!line && env_flag_enabled("KODA_TUI_FULL")) {
                return tui_full::run_tui_full(cfg).await;
            }
            return run_tui(cfg).await;
        }
        None => {}
    }

    if let Some(task) = args.task.as_deref()
        && !args.nobg
    {
        return spawn_task_background(&cfg, task).await;
    }

    let runtime = build_runtime(cfg.clone())?;
    if let Some(n) = args.llm_no {
        runtime.next_llm(n)?;
    }
    if let Some(rule) = args.reflect_rule {
        return run_reflect_mode(runtime, cfg, rule).await;
    }
    if let Some(task) = args.task {
        run_task_mode(runtime, cfg, task, args.input).await
    } else {
        let input = args
            .input
            .context("provide --input, --task, or a subcommand")?;
        let out = runtime.put_task(input).await?;
        print!("{out}");
        Ok(())
    }
}

async fn spawn_task_background(cfg: &AgentConfig, task: &str) -> Result<()> {
    let dir = task_dir(cfg, task);
    fs::create_dir_all(&dir)?;
    let stdout = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("stdout.log"))?;
    let stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("stderr.log"))?;
    let exe = env::current_exe()?;
    let mut child = StdCommand::new(exe);
    child.current_dir(&cfg.workspace_dir);
    for arg in env::args_os().skip(1) {
        child.arg(arg);
    }
    child
        .arg("--nobg")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let child = child.spawn().context("spawn background task process")?;
    println!("{}", child.id());
    Ok(())
}

fn current_exe_dir() -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

fn run_doctor(root: &Path, path_options: AgentPathOptions, json_output: bool) -> Result<()> {
    let paths = resolve_agent_paths_with_options(root, path_options.clone());
    let env_path = root.join(".env");
    let home_env_path = paths.home_dir.join(".env");
    let resource_env_path = paths.resource_dir.join(".env");
    let python = doctor_python(&paths.home_dir, PythonPurpose::AgentHelper);
    let llm = AgentConfig::from_env_with_path_options(root.to_path_buf(), path_options)
        .ok()
        .map(|cfg| {
            serde_json::json!({
                "model": cfg.openai_model,
                "api_style": cfg.llm_api_style,
                "stream": cfg.stream,
                "timeout_secs": cfg.timeout_secs,
                "connect_timeout_secs": cfg.connect_timeout_secs,
            })
        });
    let report = serde_json::json!({
        "core": {
            "home_dir": paths.home_dir.display().to_string(),
            "workspace_dir": paths.workspace_dir.display().to_string(),
            "resource_dir": paths.resource_dir.display().to_string(),
            "temp_dir": paths.temp_dir.display().to_string(),
            "memory_dir": paths.memory_dir.display().to_string(),
            "logs_dir": paths.logs_dir.display().to_string(),
            "sessions_dir": paths.sessions_dir.display().to_string(),
            "browser_dir": paths.browser_dir.display().to_string(),
            "env_file": env_path.exists() || home_env_path.exists() || resource_env_path.exists(),
            "env_keys": {
                "OPENAI_BASE_URL": env_key_available_any(&[&env_path, &home_env_path, &resource_env_path], "OPENAI_BASE_URL"),
                "OPENAI_API_KEY": env_key_available_any(&[&env_path, &home_env_path, &resource_env_path], "OPENAI_API_KEY"),
                "OPENAI_MODEL": env_key_available_any(&[&env_path, &home_env_path, &resource_env_path], "OPENAI_MODEL"),
                "OPENAI_STREAM": env_key_available_any(&[&env_path, &home_env_path, &resource_env_path], "OPENAI_STREAM"),
            }
        },
        "llm": llm,
        "python": python,
        "resources": resource_doctor_report(&paths.resource_dir, &paths.home_dir),
    });
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Core");
        println!("  home: {}", paths.home_dir.display());
        println!("  workspace: {}", paths.workspace_dir.display());
        println!("  resources: {}", paths.resource_dir.display());
        println!("  temp: {}", paths.temp_dir.display());
        println!("  memory: {}", paths.memory_dir.display());
        println!("  logs: {}", paths.logs_dir.display());
        println!(
            "  .env: {}",
            if env_path.exists() || home_env_path.exists() || resource_env_path.exists() {
                "found"
            } else {
                "missing"
            }
        );
        if let Some(llm) = report.get("llm").filter(|v| !v.is_null()) {
            println!("\nLLM");
            println!(
                "  model: {}",
                llm.get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
            );
            println!(
                "  api_style: {}",
                llm.get("api_style")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
            );
            println!(
                "  stream: {}",
                llm.get("stream")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            );
        }
        println!("\nPython");
        let python = report.get("python").unwrap();
        if python
            .get("available")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let runtime = python.get("runtime").and_then(serde_json::Value::as_object);
            let command = runtime
                .and_then(|r| r.get("command"))
                .and_then(|c| c.get("program"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let version = runtime
                .and_then(|r| r.get("version"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            println!("  status: ok");
            println!("  executable: {command}");
            println!("  version: {version}");
        } else {
            println!("  status: unavailable");
            println!("  fix: {}", python_unavailable_message());
        }
        let resources = report.get("resources").unwrap();
        let source_ok = resources
            .get("source")
            .and_then(|v| v.get("ok"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let home_ok = resources
            .get("home")
            .and_then(|v| v.get("ok"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        println!("\nResources");
        println!("  source: {}", if source_ok { "ok" } else { "missing" });
        println!("  home: {}", if home_ok { "ok" } else { "missing" });
    }
    Ok(())
}

fn run_bootstrap_python(
    root: &Path,
    extras: &[String],
    recreate: bool,
    repair: bool,
    dry_run: bool,
    offline: bool,
) -> Result<()> {
    let extras = parse_python_extras(extras)?;
    let report = bootstrap_managed_python(root, &extras, recreate, repair, dry_run, offline)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn run_python_env(command: &PythonEnvCommand) -> Result<()> {
    match command {
        PythonEnvCommand::Remove => {
            let report = remove_managed_python()?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}

fn run_resources(
    root: &Path,
    path_options: AgentPathOptions,
    command: &ResourceCommand,
) -> Result<()> {
    let paths = resolve_agent_paths_with_options(root, path_options);
    match command {
        ResourceCommand::Install {
            source,
            repair,
            dry_run,
        } => {
            let source = source.as_deref().unwrap_or(&paths.resource_dir);
            let report = install_resources(source, &paths.home_dir, *repair, *dry_run)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        ResourceCommand::Doctor { json } => {
            let report = resource_doctor_report(&paths.resource_dir, &paths.home_dir);
            if *json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("Resources");
                println!("  source: {}", paths.resource_dir.display());
                println!("  home: {}", paths.home_dir.join("resources").display());
                println!(
                    "  source markers: {}",
                    if report["source"]["ok"].as_bool().unwrap_or(false) {
                        "ok"
                    } else {
                        "missing"
                    }
                );
                println!(
                    "  home markers: {}",
                    if report["home"]["ok"].as_bool().unwrap_or(false) {
                        "ok"
                    } else {
                        "missing"
                    }
                );
            }
        }
    }
    Ok(())
}

struct UpdateRequest<'a> {
    repo: Option<&'a str>,
    version: &'a str,
    prefix: Option<&'a Path>,
    dry_run: bool,
    check: bool,
    json: bool,
    repair_resources: bool,
    bootstrap_python: bool,
}

async fn run_update(
    root: &Path,
    path_options: AgentPathOptions,
    request: UpdateRequest<'_>,
) -> Result<()> {
    let paths = resolve_agent_paths_with_options(root, path_options);
    let repo = request
        .repo
        .map(str::to_string)
        .or_else(|| env::var("KODA_AGENT_REPO").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_REPO.to_string());
    validate_repo_slug(&repo)?;
    let platform = current_update_platform()?;
    let urls = release_urls(&repo, request.version, &platform);
    let install_dir = update_install_dir(request.prefix)?;
    let binary_name = if cfg!(windows) {
        "koda-agent.exe"
    } else {
        "koda-agent"
    };
    let target_binary = install_dir.join(binary_name);
    if request.check {
        let latest = fetch_latest_release(&repo).await?;
        let report = update_check_report(&repo, &latest, &platform);
        if request.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_update_check_text(&report);
        }
        return Ok(());
    }
    if request.dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": true,
                "current_version": env!("CARGO_PKG_VERSION"),
                "repo": repo,
                "version": request.version,
                "target": platform.target,
                "archive": platform.archive_name,
                "download_url": urls.archive,
                "checksum_url": urls.checksums,
                "install_dir": install_dir.display().to_string(),
                "binary": target_binary.display().to_string(),
                "home": paths.home_dir.display().to_string(),
                "repair_resources": request.repair_resources,
                "bootstrap_python": request.bootstrap_python,
            }))?
        );
        return Ok(());
    }

    fs::create_dir_all(&install_dir)
        .with_context(|| format!("create install directory {}", install_dir.display()))?;
    fs::create_dir_all(&paths.home_dir)
        .with_context(|| format!("create Koda home {}", paths.home_dir.display()))?;
    let tmp = tempfile::Builder::new()
        .prefix("koda-agent-update-")
        .tempdir()
        .context("create update tempdir")?;
    let archive_path = tmp.path().join(&platform.archive_name);
    let archive = download_bytes(&urls.archive).await?;
    verify_release_checksum(
        &archive,
        &download_text(&urls.checksums).await?,
        &platform.archive_name,
    )?;
    fs::write(&archive_path, &archive)
        .with_context(|| format!("write archive {}", archive_path.display()))?;
    extract_release_archive(&archive_path, tmp.path(), platform.archive_kind)?;
    let extracted_binary = tmp.path().join(binary_name);
    if !extracted_binary.is_file() {
        bail!(
            "release archive did not contain expected binary {}",
            binary_name
        );
    }
    let binary_update = install_updated_binary(&extracted_binary, &target_binary)?;
    let mut resource_report = serde_json::Value::Null;
    if request.repair_resources {
        let resources = tmp.path().join("resources");
        if resources.is_dir() {
            resource_report = install_resources(&resources, &paths.home_dir, true, false)?;
        } else {
            bail!("release archive did not contain resources/");
        }
    }
    let mut python_report = serde_json::Value::Null;
    if request.bootstrap_python {
        let resource_root = paths.home_dir.join("resources");
        python_report = serde_json::to_value(bootstrap_managed_python(
            &resource_root,
            &[PythonExtra::Core],
            false,
            true,
            false,
            false,
        )?)?;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "updated": true,
            "previous_version": env!("CARGO_PKG_VERSION"),
            "repo": repo,
            "version": request.version,
            "target": platform.target,
            "binary": target_binary.display().to_string(),
            "binary_update": binary_update,
            "resources": resource_report,
            "python": python_report,
        }))?
    );
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    prerelease: bool,
    draft: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    TarGz,
    Zip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UpdatePlatform {
    target: String,
    archive_name: String,
    archive_kind: ArchiveKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseUrls {
    archive: String,
    checksums: String,
}

fn current_update_platform() -> Result<UpdatePlatform> {
    update_platform_for(env::consts::OS, env::consts::ARCH)
}

fn update_platform_for(os: &str, arch: &str) -> Result<UpdatePlatform> {
    let target = match (os, arch) {
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        _ => bail!("unsupported update platform: {os}/{arch}"),
    };
    let archive_kind = if os == "windows" {
        ArchiveKind::Zip
    } else {
        ArchiveKind::TarGz
    };
    let suffix = match archive_kind {
        ArchiveKind::TarGz => "tar.gz",
        ArchiveKind::Zip => "zip",
    };
    Ok(UpdatePlatform {
        target: target.to_string(),
        archive_name: format!("koda-agent-{target}.{suffix}"),
        archive_kind,
    })
}

fn release_urls(repo: &str, version: &str, platform: &UpdatePlatform) -> ReleaseUrls {
    let base = if version == "latest" {
        format!("https://github.com/{repo}/releases/latest/download")
    } else {
        format!("https://github.com/{repo}/releases/download/{version}")
    };
    ReleaseUrls {
        archive: format!("{base}/{}", platform.archive_name),
        checksums: format!("{base}/SHA256SUMS"),
    }
}

async fn fetch_latest_release(repo: &str) -> Result<GithubRelease> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let response = reqwest::Client::new()
        .get(&url)
        .header(reqwest::header::USER_AGENT, "koda-agent-updater")
        .send()
        .await
        .with_context(|| format!("check latest release {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("check latest release {url} failed with {status}");
    }
    response
        .json::<GithubRelease>()
        .await
        .with_context(|| format!("parse latest release response from {url}"))
}

fn update_check_report(
    repo: &str,
    latest: &GithubRelease,
    platform: &UpdatePlatform,
) -> serde_json::Value {
    let current = env!("CARGO_PKG_VERSION");
    let latest_version = latest.tag_name.trim_start_matches('v');
    let cmp = compare_version_like(current, latest_version);
    let update_available = cmp
        .map(|ord| ord.is_lt())
        .unwrap_or(current != latest_version);
    let urls = release_urls(repo, &latest.tag_name, platform);
    serde_json::json!({
        "repo": repo,
        "current_version": current,
        "latest_version": latest_version,
        "latest_tag": latest.tag_name,
        "update_available": update_available,
        "comparison": cmp.map(|ord| match ord {
            std::cmp::Ordering::Less => "older",
            std::cmp::Ordering::Equal => "equal",
            std::cmp::Ordering::Greater => "newer",
        }),
        "target": platform.target,
        "archive": platform.archive_name,
        "download_url": urls.archive,
        "checksum_url": urls.checksums,
        "release_url": latest.html_url,
        "prerelease": latest.prerelease,
        "draft": latest.draft,
    })
}

fn print_update_check_text(report: &serde_json::Value) {
    let current = report["current_version"].as_str().unwrap_or("unknown");
    let latest_tag = report["latest_tag"].as_str().unwrap_or("unknown");
    let target = report["target"].as_str().unwrap_or("unknown");
    let release_url = report["release_url"].as_str().unwrap_or("");
    let update_available = report["update_available"].as_bool().unwrap_or(false);
    println!("Koda Agent update check");
    println!("  current: v{current}");
    println!("  latest: {latest_tag}");
    println!("  target: {target}");
    if update_available {
        println!("  status: update available");
        println!("  run: koda-agent update --version {latest_tag}");
    } else {
        println!("  status: already up to date");
    }
    if !release_url.is_empty() {
        println!("  release: {release_url}");
    }
}

fn compare_version_like(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let left = parse_version_like(left)?;
    let right = parse_version_like(right)?;
    Some(left.cmp(&right))
}

fn parse_version_like(version: &str) -> Option<Vec<u64>> {
    let core = version
        .trim_start_matches('v')
        .split_once('-')
        .map(|(core, _)| core)
        .unwrap_or(version.trim_start_matches('v'));
    let mut out = Vec::new();
    for part in core.split('.') {
        out.push(part.parse().ok()?);
    }
    Some(out)
}

fn validate_repo_slug(repo: &str) -> Result<()> {
    let valid = repo.split_once('/').is_some_and(|(owner, name)| {
        !owner.is_empty()
            && !name.is_empty()
            && [owner, name].iter().all(|part| {
                part.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
            })
    });
    if !valid {
        bail!("invalid GitHub repo slug: {repo}; expected OWNER/REPO");
    }
    Ok(())
}

fn update_install_dir(prefix: Option<&Path>) -> Result<PathBuf> {
    if let Some(prefix) = prefix {
        return Ok(prefix.join("bin"));
    }
    let exe = env::current_exe().context("resolve current executable")?;
    exe.parent()
        .map(Path::to_path_buf)
        .context("current executable has no parent")
}

async fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("download {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("download {url} failed with {status}");
    }
    Ok(response.bytes().await?.to_vec())
}

async fn download_text(url: &str) -> Result<String> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("download {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("download {url} failed with {status}");
    }
    Ok(response.text().await?)
}

fn verify_release_checksum(bytes: &[u8], checksums: &str, archive_name: &str) -> Result<()> {
    let expected = checksum_for_archive(checksums, archive_name)
        .with_context(|| format!("{archive_name} not found in SHA256SUMS"))?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if expected != actual {
        bail!("checksum mismatch for {archive_name}: expected {expected}, got {actual}");
    }
    Ok(())
}

fn checksum_for_archive(checksums: &str, archive_name: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == archive_name || file.ends_with(&format!("/{archive_name}")))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn extract_release_archive(path: &Path, dest: &Path, kind: ArchiveKind) -> Result<()> {
    match kind {
        ArchiveKind::TarGz => {
            let file =
                fs::File::open(path).with_context(|| format!("open archive {}", path.display()))?;
            let decoder = flate2::read::GzDecoder::new(file);
            let mut archive = tar::Archive::new(decoder);
            archive
                .unpack(dest)
                .with_context(|| format!("extract archive {}", path.display()))?;
        }
        ArchiveKind::Zip => {
            let file =
                fs::File::open(path).with_context(|| format!("open archive {}", path.display()))?;
            let mut archive = zip::ZipArchive::new(file)?;
            archive
                .extract(dest)
                .with_context(|| format!("extract archive {}", path.display()))?;
        }
    }
    Ok(())
}

fn install_updated_binary(src: &Path, dst: &Path) -> Result<serde_json::Value> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(windows)]
    {
        let current = env::current_exe().ok().and_then(|p| p.canonicalize().ok());
        let target = dst.canonicalize().ok();
        if current.is_some() && current == target {
            let pending = dst.with_extension("exe.new");
            fs::copy(src, &pending)
                .with_context(|| format!("copy pending binary {}", pending.display()))?;
            let script = dst.with_file_name("koda-agent-apply-update.ps1");
            fs::write(
                &script,
                format!(
                    "$ErrorActionPreference='Stop'\nWait-Process -Id {}\nMove-Item -Force '{}' '{}'\n",
                    std::process::id(),
                    pending.display(),
                    dst.display()
                ),
            )?;
            StdCommand::new("powershell")
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    &script.display().to_string(),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("spawn Windows deferred updater")?;
            return Ok(serde_json::json!({
                "mode": "deferred_windows_replace",
                "pending_binary": pending.display().to_string(),
                "script": script.display().to_string(),
            }));
        }
    }
    fs::copy(src, dst).with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(dst)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(dst, perms)?;
    }
    Ok(serde_json::json!({ "mode": "direct_replace" }))
}

fn parse_python_extras(values: &[String]) -> Result<Vec<PythonExtra>> {
    let mut out = Vec::new();
    if values.is_empty() {
        return Ok(vec![PythonExtra::Core]);
    }
    for value in values {
        if value.eq_ignore_ascii_case("all") {
            for extra in [
                PythonExtra::Core,
                PythonExtra::Ocr,
                PythonExtra::Automation,
                PythonExtra::Dev,
            ] {
                if !out.contains(&extra) {
                    out.push(extra);
                }
            }
            continue;
        }
        let Some(extra) = PythonExtra::parse(value) else {
            bail!("unknown Python extra: {value}; expected core, ocr, automation, dev, all");
        };
        if !out.contains(&extra) {
            out.push(extra);
        }
    }
    if !out.contains(&PythonExtra::Core) {
        out.insert(0, PythonExtra::Core);
    }
    Ok(out)
}

fn env_key_available(env_path: &Path, key: &str) -> bool {
    if env::var(key).is_ok_and(|value| !value.trim().is_empty()) {
        return true;
    }
    dotenvy::from_path_iter(env_path)
        .ok()
        .and_then(|iter| {
            iter.filter_map(|item| item.ok())
                .find(|(k, v)| k == key && !v.trim().is_empty())
        })
        .is_some()
}

fn env_key_available_any(env_paths: &[&Path], key: &str) -> bool {
    env::var(key).is_ok_and(|value| !value.trim().is_empty())
        || env_paths.iter().any(|path| env_key_available(path, key))
}

fn install_resources(
    source_root: &Path,
    home_dir: &Path,
    repair: bool,
    dry_run: bool,
) -> Result<serde_json::Value> {
    let dest_root = home_dir.join("resources");
    if paths_same_when_existing(source_root, &dest_root) {
        return Ok(serde_json::json!({
            "source": source_root.display().to_string(),
            "home": home_dir.display().to_string(),
            "destination": dest_root.display().to_string(),
            "repair": repair,
            "dry_run": dry_run,
            "copied": [],
            "skipped": ["source is already the home resources directory"],
            "missing": [],
            "doctor": resource_doctor_report(source_root, home_dir),
        }));
    }
    let mut copied = Vec::new();
    let mut skipped = Vec::new();
    let mut missing = Vec::new();
    for name in ["assets", "memory"] {
        let src = source_root.join(name);
        if src.exists() {
            copy_resource_tree(
                &src,
                &dest_root.join(name),
                &dest_root,
                repair,
                dry_run,
                &mut copied,
                &mut skipped,
            )?;
        } else {
            missing.push(src.display().to_string());
        }
    }
    if let Ok(entries) = fs::read_dir(source_root) {
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("requirements-python-") && name.ends_with(".txt") {
                copy_resource_file(
                    &entry.path(),
                    &dest_root.join(name.as_ref()),
                    &dest_root,
                    repair,
                    dry_run,
                    &mut copied,
                    &mut skipped,
                )?;
            }
        }
    } else {
        missing.push(source_root.display().to_string());
    }
    if !dry_run {
        fs::create_dir_all(home_dir.join("browser"))?;
    }
    prepare_browser_bridge(
        &dest_root,
        home_dir,
        repair,
        dry_run,
        &mut copied,
        &mut skipped,
    )?;
    Ok(serde_json::json!({
        "source": source_root.display().to_string(),
        "home": home_dir.display().to_string(),
        "destination": dest_root.display().to_string(),
        "repair": repair,
        "dry_run": dry_run,
        "copied": copied,
        "skipped": skipped,
        "missing": missing,
        "doctor": resource_doctor_report(source_root, home_dir),
    }))
}

fn prepare_browser_bridge(
    resources_dir: &Path,
    home_dir: &Path,
    repair: bool,
    dry_run: bool,
    copied: &mut Vec<String>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    let src = resources_dir.join("assets/tmwd_cdp_bridge");
    if !src.exists() {
        return Ok(());
    }
    let dst = home_dir.join("browser/tmwd_cdp_bridge");
    copy_resource_tree(&src, &dst, home_dir, repair, dry_run, copied, skipped)?;
    let config = dst.join("config.js");
    if config.exists() {
        skipped.push("browser/tmwd_cdp_bridge/config.js".into());
        return Ok(());
    }
    copied.push("browser/tmwd_cdp_bridge/config.js".into());
    if dry_run {
        return Ok(());
    }
    fs::create_dir_all(&dst)?;
    fs::write(config, format!("const TID = '{}';\n", browser_bridge_tid()))?;
    Ok(())
}

fn browser_bridge_tid() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "__ljq_{:06x}",
        (nanos ^ std::process::id() as u128) & 0x00ff_ffff
    )
}

fn paths_same_when_existing(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn copy_resource_tree(
    src: &Path,
    dst: &Path,
    dest_root: &Path,
    repair: bool,
    dry_run: bool,
    copied: &mut Vec<String>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    for entry in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if should_skip_resource(&src_path) {
            skipped.push(src_path.display().to_string());
            continue;
        }
        if file_type.is_dir() {
            copy_resource_tree(
                &src_path, &dst_path, dest_root, repair, dry_run, copied, skipped,
            )?;
        } else if file_type.is_file() {
            copy_resource_file(
                &src_path, &dst_path, dest_root, repair, dry_run, copied, skipped,
            )?;
        }
    }
    Ok(())
}

fn copy_resource_file(
    src: &Path,
    dst: &Path,
    dest_root: &Path,
    repair: bool,
    dry_run: bool,
    copied: &mut Vec<String>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    let rel = dst
        .strip_prefix(dest_root)
        .unwrap_or(dst)
        .display()
        .to_string();
    if dst.exists() && !repair {
        skipped.push(rel);
        return Ok(());
    }
    copied.push(rel);
    if dry_run {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, dst).with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    Ok(())
}

fn should_skip_resource(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if matches!(
        name,
        "config.js"
            | "global_mem.txt"
            | "global_mem_insight.txt"
            | "file_access_stats.json"
            | "long_term_updates.jsonl"
            | "pending_long_term_updates.md"
            | "all_histories.txt"
    ) || name.ends_with(".bak")
        || name.ends_with(".zip")
    {
        return true;
    }
    if path.components().any(|c| c.as_os_str() == "memory") {
        return !allowed_memory_resource(path, name);
    }
    false
}

fn allowed_memory_resource(path: &Path, name: &str) -> bool {
    const TOP_LEVEL: &[&str] = &[
        "adb_ui.py",
        "autonomous_operation_sop.md",
        "chat_html_debug_sop.md",
        "github_contribution_sop.md",
        "goal_mode_sop.md",
        "keychain.py",
        "ljqCtrl.py",
        "ljqCtrl_sop.md",
        "memory_cleanup_sop.md",
        "memory_management_sop.md",
        "ocr_utils.py",
        "plan_sop.md",
        "procmem_scanner.py",
        "procmem_scanner_sop.md",
        "scheduled_task_sop.md",
        "subagent.md",
        "supervisor_sop.md",
        "tmwebdriver_sop.md",
        "ui_detect.py",
        "verify_sop.md",
        "vision_api.py",
        "vision_api.template.py",
        "vision_sop.md",
        "vue3_component_sop.md",
        "web_setup_sop.md",
    ];
    if TOP_LEVEL.contains(&name) {
        return true;
    }
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("L4_raw_sessions" | "autonomous_operation_sop" | "skill_search")
        )
    })
}

fn resource_doctor_report(resource_dir: &Path, home_dir: &Path) -> serde_json::Value {
    let home_resources = home_dir.join("resources");
    serde_json::json!({
        "source": resource_marker_report(resource_dir),
        "home": resource_marker_report(&home_resources),
        "browser": {
            "extension_dir": home_dir.join("browser/tmwd_cdp_bridge").display().to_string(),
            "installed": home_dir.join("browser/tmwd_cdp_bridge/manifest.json").is_file(),
            "runtime_config": home_dir.join("browser/tmwd_cdp_bridge/config.js").is_file(),
        }
    })
}

fn resource_marker_report(dir: &Path) -> serde_json::Value {
    let assets = dir.join("assets");
    let memory = dir.join("memory");
    let markers = serde_json::json!({
        "tools_schema": assets.join("tools_schema.json").is_file(),
        "sys_prompt": assets.join("sys_prompt.txt").is_file(),
        "simphtml": assets.join("simphtml_opt.js").is_file(),
        "tmwd_cdp_bridge": assets.join("tmwd_cdp_bridge/manifest.json").is_file(),
        "memory_sop": memory.join("memory_management_sop.md").is_file(),
        "requirements_core": dir.join("requirements-python-core.txt").is_file(),
    });
    let ok = markers
        .as_object()
        .map(|m| m.values().all(|v| v.as_bool().unwrap_or(false)))
        .unwrap_or(false);
    serde_json::json!({
        "path": dir.display().to_string(),
        "ok": ok,
        "markers": markers,
    })
}

#[derive(Debug, Clone)]
struct ReflectProbe {
    task: Option<String>,
    interval: u64,
    once: bool,
}

fn parse_reflect_probe_output(stdout: &str) -> Result<ReflectProbe> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .with_context(|| format!("parse reflect script JSON output: {stdout}"))?;
    if value.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        anyhow::bail!(
            "{}",
            value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("reflect check() failed")
        );
    }
    Ok(ReflectProbe {
        task: value
            .get("task")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        interval: value
            .get("interval")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(5),
        once: value
            .get("once")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

async fn run_reflect_mode(runtime: AgentRuntime, cfg: AgentConfig, script: String) -> Result<()> {
    let script_path = Path::new(&script).to_path_buf();
    let script_name = script_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("reflect")
        .to_string();
    println!("[Reflect] loaded {}", script_path.display());
    let mut last_l4_archive: Option<Instant> = None;
    loop {
        if last_l4_archive.is_none_or(|t| t.elapsed() > Duration::from_secs(43_200)) {
            last_l4_archive = Some(Instant::now());
            match archive_l4_sessions(&cfg, None, false) {
                Ok(report) if report.new_sessions > 0 => {
                    println!("[Reflect] L4 archive: {} new sessions", report.new_sessions);
                }
                Ok(_) => {}
                Err(e) => println!("[Reflect] L4 archive error: {e:#}"),
            }
        }
        let probe = match reflect_check_once(&script_path, &cfg).await {
            Ok(probe) => probe,
            Err(e) => {
                println!("[Reflect] check() error: {e:#}");
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        sleep(Duration::from_secs(probe.interval)).await;
        let Some(task) = probe.task.clone() else {
            if probe.once {
                println!("[Reflect] ONCE=True, exiting.");
                break;
            }
            continue;
        };
        if task.trim() == "/exit" {
            break;
        }
        println!(
            "[Reflect] triggered: {}",
            task.chars().take(80).collect::<String>()
        );
        let result = match timeout(Duration::from_secs(180), runtime.put_task(task)).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                if probe.once {
                    return Err(e);
                }
                format!("[ERROR] {e:#}")
            }
            Err(e) => {
                if probe.once {
                    anyhow::bail!("reflect drain timeout: {e}");
                }
                format!("[ERROR] reflect drain timeout: {e}")
            }
        };
        println!("{result}");
        write_reflect_log(&cfg, &script_name, &result)?;
        if let Err(e) = reflect_on_done(&script_path, &cfg, &result).await {
            println!("[Reflect] on_done error: {e:#}");
        }
        if probe.once {
            println!("[Reflect] ONCE=True, exiting.");
            break;
        }
    }
    Ok(())
}

async fn reflect_check_once(script: &Path, cfg: &AgentConfig) -> Result<ReflectProbe> {
    if is_native_autonomous_reflect(script) {
        return Ok(reflect_check_autonomous());
    }
    if is_native_agent_team_reflect(script)
        || matches!(
            json_reflect_kind(script).as_deref(),
            Some("agent_team_worker" | "team_worker")
        )
    {
        return reflect_check_agent_team(&cfg.workspace_dir, script).await;
    }
    if is_native_goal_reflect(script) {
        return reflect_check_goal(&cfg.workspace_dir, chrono::Local::now().timestamp());
    }
    if is_native_scheduler_reflect(script) {
        return reflect_check_scheduler(&cfg.workspace_dir, chrono::Local::now().naive_local());
    }
    if script.extension().and_then(|s| s.to_str()) == Some("json") {
        return reflect_check_json(script, &cfg.workspace_dir);
    }
    let script = script.to_path_buf();
    let root = cfg.workspace_dir.clone();
    tokio::task::spawn_blocking(move || {
        let Some(pybin) = resolve_python(&root, PythonPurpose::AgentHelper) else {
            bail!(
                "{} Use a native JSON reflect rule if Python is unavailable.",
                python_unavailable_message()
            );
        };
        let py = r#"
import importlib.util, json, sys, traceback
path = sys.argv[1]
try:
    spec = importlib.util.spec_from_file_location('reflect_script', path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    task = mod.check()
    print(json.dumps({
        'ok': True,
        'task': task,
        'interval': getattr(mod, 'INTERVAL', 5),
        'once': bool(getattr(mod, 'ONCE', False)),
    }, ensure_ascii=False))
except Exception:
    print(json.dumps({'ok': False, 'error': traceback.format_exc()}, ensure_ascii=False))
"#;
        let mut cmd = StdCommand::new(&pybin.command.program);
        cmd.args(&pybin.command.args);
        let output = cmd
            .arg("-c")
            .arg(py)
            .arg(&script)
            .output()
            .with_context(|| format!("run reflect check {}", script.display()))?;
        if !output.status.success() {
            anyhow::bail!(
                "reflect python exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        parse_reflect_probe_output(&String::from_utf8_lossy(&output.stdout))
    })
    .await?
}

async fn reflect_on_done(script: &Path, cfg: &AgentConfig, result: &str) -> Result<()> {
    if is_native_agent_team_reflect(script)
        || matches!(
            json_reflect_kind(script).as_deref(),
            Some("agent_team_worker" | "team_worker")
        )
    {
        reflect_agent_team_on_done(&cfg.workspace_dir, chrono::Local::now().timestamp())?;
        return Ok(());
    }
    if is_native_goal_reflect(script)
        || matches!(
            json_reflect_kind(script).as_deref(),
            Some("goal" | "goal_mode")
        )
    {
        reflect_goal_on_done(
            &goal_state_path(&cfg.workspace_dir),
            chrono::Local::now().timestamp(),
        )?;
        return Ok(());
    }
    if script.extension().and_then(|s| s.to_str()) == Some("json")
        || is_native_scheduler_reflect(script)
    {
        return Ok(());
    }
    let script = script.to_path_buf();
    let result = result.to_string();
    let root = cfg.workspace_dir.clone();
    tokio::task::spawn_blocking(move || {
        let Some(pybin) = resolve_python(&root, PythonPurpose::AgentHelper) else {
            bail!(
                "Python interpreter not found for reflect on_done. {}",
                python_unavailable_message()
            );
        };
        let py = r#"
import importlib.util, sys, traceback
path = sys.argv[1]
result = sys.stdin.read()
try:
    spec = importlib.util.spec_from_file_location('reflect_script', path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    cb = getattr(mod, 'on_done', None)
    if cb:
        cb(result)
except Exception:
    traceback.print_exc()
    raise
"#;
        let mut cmd = StdCommand::new(&pybin.command.program);
        cmd.args(&pybin.command.args);
        let mut child = cmd
            .arg("-c")
            .arg(py)
            .arg(&script)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("run reflect on_done {}", script.display()))?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(result.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            anyhow::bail!(
                "reflect on_done exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    })
    .await?
}

fn reflect_check_json(path: &Path, root_dir: &Path) -> Result<ReflectProbe> {
    let value: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse JSON reflect rule {}", path.display()))?;
    let kind = value
        .get("kind")
        .or_else(|| value.get("type"))
        .and_then(serde_json::Value::as_str);
    if matches!(kind, Some("scheduler" | "scheduled_tasks")) {
        return reflect_check_scheduler(root_dir, chrono::Local::now().naive_local());
    }
    if matches!(kind, Some("goal" | "goal_mode")) {
        return reflect_check_goal(root_dir, chrono::Local::now().timestamp());
    }
    if matches!(kind, Some("autonomous" | "auto")) {
        return Ok(reflect_check_autonomous());
    }
    let interval = value
        .get("interval")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(5);
    let once = value
        .get("once")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let task = value
        .get("task")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    if let Some(watch) = value.get("watch_file").and_then(serde_json::Value::as_str) {
        let watch_path = if Path::new(watch).is_absolute() {
            Path::new(watch).to_path_buf()
        } else {
            path.parent().unwrap_or_else(|| Path::new(".")).join(watch)
        };
        let trigger = value
            .get("trigger")
            .or_else(|| value.get("trigger_on"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("exists");
        let fired = match trigger {
            "exists" => watch_path.exists(),
            "nonempty" => fs::metadata(&watch_path)
                .map(|m| m.len() > 0)
                .unwrap_or(false),
            other => bail!("unsupported JSON reflect trigger: {other}"),
        };
        return Ok(ReflectProbe {
            task: fired.then_some(task).flatten(),
            interval,
            once,
        });
    }
    Ok(ReflectProbe {
        task,
        interval,
        once,
    })
}

fn reflect_check_autonomous() -> ReflectProbe {
    ReflectProbe {
        task: Some(
            "[AUTO]🤖 用户已经离开超过30分钟，作为自主智能体，请阅读自动化sop，执行自动任务。"
                .into(),
        ),
        interval: 1800,
        once: false,
    }
}

fn is_native_autonomous_reflect(path: &Path) -> bool {
    matches!(
        path.file_stem().and_then(|s| s.to_str()),
        Some("autonomous" | "auto")
    ) && (path.extension().is_none() || path.extension().and_then(|s| s.to_str()) == Some("json"))
}

fn is_native_agent_team_reflect(path: &Path) -> bool {
    matches!(
        path.file_stem().and_then(|s| s.to_str()),
        Some("agent_team_worker" | "team_worker")
    ) && (path.extension().is_none() || path.extension().and_then(|s| s.to_str()) == Some("json"))
}

#[derive(Debug, Clone, Default)]
struct AgentTeamConfig {
    base_url: String,
    board_key: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct AgentTeamState {
    #[serde(default = "minus_one")]
    last_id: i64,
    #[serde(default = "minus_one")]
    last_done: i64,
}

fn minus_one() -> i64 {
    -1
}

async fn reflect_check_agent_team(root_dir: &Path, script: &Path) -> Result<ReflectProbe> {
    let cfg = load_agent_team_config(root_dir, script)?;
    if cfg.base_url.trim().is_empty() {
        return Ok(ReflectProbe {
            task: None,
            interval: 60,
            once: false,
        });
    }
    let state_path = agent_team_state_path(root_dir);
    let mut state = load_agent_team_state(&state_path)?;
    let now = chrono::Local::now().timestamp();
    if state.last_done > 0 && now - state.last_done < 120 {
        return Ok(ReflectProbe {
            task: Some(agent_team_prompt(&cfg)),
            interval: 60,
            once: false,
        });
    }
    let url = format!("{}/posts?limit=10", cfg.base_url.trim_end_matches('/'));
    let resp = match reqwest::Client::new()
        .get(url)
        .header("X-API-Key", &cfg.board_key)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .context("poll agent team board")
    {
        Ok(resp) => resp,
        Err(e) => {
            write_reflect_runtime_log(
                state_path.parent().unwrap_or_else(|| Path::new(".")),
                &format!("WARN agent_team board poll failed: {e:#}"),
            )?;
            return Ok(ReflectProbe {
                task: None,
                interval: 60,
                once: false,
            });
        }
    };
    let posts = match resp
        .json::<serde_json::Value>()
        .await
        .context("decode agent team board posts")
    {
        Ok(posts) => posts,
        Err(e) => {
            write_reflect_runtime_log(
                state_path.parent().unwrap_or_else(|| Path::new(".")),
                &format!("WARN agent_team board decode failed: {e:#}"),
            )?;
            return Ok(ReflectProbe {
                task: None,
                interval: 60,
                once: false,
            });
        }
    };
    let Some(max_id) = agent_team_max_post_id(&posts) else {
        return Ok(ReflectProbe {
            task: None,
            interval: 60,
            once: false,
        });
    };
    if max_id <= state.last_id {
        return Ok(ReflectProbe {
            task: None,
            interval: 60,
            once: false,
        });
    }
    state.last_id = max_id;
    save_agent_team_state(&state_path, &state)?;
    Ok(ReflectProbe {
        task: Some(agent_team_prompt(&cfg)),
        interval: 60,
        once: false,
    })
}

fn load_agent_team_config(root_dir: &Path, script: &Path) -> Result<AgentTeamConfig> {
    let from_json =
        if script.extension().and_then(|s| s.to_str()) == Some("json") && script.exists() {
            fs::read_to_string(script)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        } else {
            None
        };
    if let Some(value) = from_json {
        let base_url = value
            .get("base_url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let board_key = value
            .get("board_key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !base_url.is_empty() || !board_key.is_empty() {
            return Ok(AgentTeamConfig {
                base_url,
                board_key,
            });
        }
    }
    let candidates = [
        script
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("agent_team_setting.json"),
        root_dir.join("reflect/agent_team_setting.json"),
    ];
    for path in candidates {
        if !path.exists() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        return Ok(AgentTeamConfig {
            base_url: value
                .get("base_url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            board_key: value
                .get("board_key")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Ok(AgentTeamConfig::default())
}

fn agent_team_state_path(root_dir: &Path) -> PathBuf {
    root_dir.join("temp/agent_team_worker_state.json")
}

fn load_agent_team_state(path: &Path) -> Result<AgentTeamState> {
    if !path.exists() {
        return Ok(AgentTeamState {
            last_id: -1,
            last_done: -1,
        });
    }
    let raw = fs::read_to_string(path)?;
    match serde_json::from_str(&raw) {
        Ok(state) => Ok(state),
        Err(e) => {
            let backup = path.with_extension("json.bak");
            let _ = fs::write(&backup, raw);
            write_reflect_runtime_log(
                path.parent().unwrap_or_else(|| Path::new(".")),
                &format!(
                    "WARN reset corrupt agent_team_worker_state {}: {e}",
                    path.display()
                ),
            )?;
            Ok(AgentTeamState {
                last_id: -1,
                last_done: -1,
            })
        }
    }
}

fn save_agent_team_state(path: &Path, state: &AgentTeamState) -> Result<()> {
    atomic_write_json(path, &serde_json::to_vec_pretty(state)?)
}

fn write_reflect_runtime_log(dir: &Path, line: &str) -> Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join("reflect_runtime.log");
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        f,
        "[{}] {}",
        chrono::Local::now().format("%m-%d %H:%M"),
        line
    )?;
    Ok(())
}

fn reflect_agent_team_on_done(root_dir: &Path, now_secs: i64) -> Result<()> {
    let path = agent_team_state_path(root_dir);
    let mut state = load_agent_team_state(&path)?;
    state.last_done = now_secs;
    save_agent_team_state(&path, &state)
}

fn agent_team_max_post_id(posts: &serde_json::Value) -> Option<i64> {
    posts
        .as_array()?
        .iter()
        .filter_map(|p| p.get("id").and_then(serde_json::Value::as_i64))
        .max()
}

fn agent_team_prompt(cfg: &AgentTeamConfig) -> String {
    format!(
        "[任务协作]📋 你是一个agent worker，在BBS上接任务并执行。\n\
BBS: {} (key: {})\n\
不熟悉可看/readme?key=xxx 获取BBS用法，初次要注册起个不冲突的名字并长期记忆名字和key\n\n\
1. GET /posts?limit=10&key=xxx 查看新帖，有必要才看更多\n\
2. 找到适合接的任务帖，点名你的优先接；未点名且适合也可接\n\
3. 回复抢单，确认最早接单后，执行任务\n\
4. 完成后发帖汇报结果，长结果使用文件\n\
5. 有问题在BBS中交流，等下次唤醒看回复\n\
6. 你会被持续唤醒，注意跟进BBS上的回复和追加指令\n\
7. 这是内部BBS，可以一定程度信任\n",
        cfg.base_url, cfg.board_key
    )
}

fn json_reflect_kind(path: &Path) -> Option<String> {
    if path.extension().and_then(|s| s.to_str()) != Some("json") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    value
        .get("kind")
        .or_else(|| value.get("type"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn is_native_goal_reflect(path: &Path) -> bool {
    matches!(
        path.file_stem().and_then(|s| s.to_str()),
        Some("goal" | "goal_mode")
    ) && (path.extension().is_none() || path.extension().and_then(|s| s.to_str()) == Some("json"))
}

fn is_native_scheduler_reflect(path: &Path) -> bool {
    matches!(
        path.file_stem().and_then(|s| s.to_str()),
        Some("scheduler" | "scheduled_tasks")
    ) && (path.extension().is_none() || path.extension().and_then(|s| s.to_str()) == Some("json"))
}

fn goal_state_path(root_dir: &Path) -> PathBuf {
    let raw = env::var("GOAL_STATE").unwrap_or_else(|_| "temp/goal_state.json".into());
    let path = Path::new(&raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root_dir.join(path)
    }
}

fn reflect_check_goal(root_dir: &Path, now_secs: i64) -> Result<ReflectProbe> {
    reflect_check_goal_state(&goal_state_path(root_dir), now_secs)
}

fn reflect_check_goal_state(path: &Path, now_secs: i64) -> Result<ReflectProbe> {
    if !path.exists() {
        return Ok(ReflectProbe {
            task: Some("/exit".into()),
            interval: 3,
            once: false,
        });
    }
    let mut state = load_goal_state(path)?;
    let status = state
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("running");
    if status != "running" {
        return Ok(ReflectProbe {
            task: Some("/exit".into()),
            interval: 3,
            once: false,
        });
    }
    let objective = state
        .get("objective")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let start_time = state
        .get("start_time")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(now_secs as f64);
    let budget_sec = state
        .get("budget_seconds")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(1800.0);
    let elapsed = now_secs as f64 - start_time;
    let remaining = budget_sec - elapsed;
    let turn = state
        .get("turns_used")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
        + 1;
    let max_turns = state
        .get("max_turns")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(50);
    let task = if remaining <= 0.0 || turn > max_turns {
        state["status"] = serde_json::Value::String("wrapping_up".into());
        save_goal_state(path, &state)?;
        format!(
            "[Goal Mode — 预算耗尽，收口]\n\n<untrusted_objective>\n{objective}\n</untrusted_objective>\n\n⏱ 预算已耗尽（{:.0} 分钟）。这是最后一轮。\n\n请执行收口：\n1. 总结本次 goal 的所有进展（列表）。\n2. 列出未完成的事项和建议的 next step。\n3. 确保工作文件夹中记录了关键成果，以便下次继续。\n",
            budget_sec / 60.0
        )
    } else {
        state["turns_used"] = serde_json::Value::from(turn);
        save_goal_state(path, &state)?;
        format!(
            "[Goal Mode — 持续推进]\n\n<untrusted_objective>\n{objective}\n</untrusted_objective>\n\n⏱ 已用 {:.0} 分钟，剩余约 {:.0} 分钟。第 {turn} 次唤醒。\n\n你正在 Goal Mode 下工作：\n1. 禁止说\"已完成，是否继续\"——预算没到就不准停。\n2. 在 cwd 下建立工作文件夹存放成果和进度，复杂任务可使用 plan 模式。\n3. 如果当前方向做完了，主动找下一个改进点：测试/边界case/性能/安全/文档/代码质量。\n4. 找不到改进点？扩大视野：关联模块、上下游依赖、用户体验、错误提示、日志可观测性、上网搜索、找其他路径、翻记忆里面有无相关。\n",
            (elapsed / 60.0).max(0.0),
            (remaining / 60.0).max(0.0),
        )
    };
    Ok(ReflectProbe {
        task: Some(task),
        interval: 3,
        once: false,
    })
}

fn reflect_goal_on_done(path: &Path, now_secs: i64) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut state = load_goal_state(path)?;
    if state.get("status").and_then(serde_json::Value::as_str) == Some("wrapping_up") {
        state["status"] = serde_json::Value::String("done_budget".into());
        state["end_time"] = serde_json::Value::from(now_secs);
        save_goal_state(path, &state)?;
    }
    Ok(())
}

fn save_goal_state(path: &Path, state: &serde_json::Value) -> Result<()> {
    atomic_write_json(path, &serde_json::to_vec_pretty(state)?)
}

fn load_goal_state(path: &Path) -> Result<serde_json::Value> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    match serde_json::from_str(&raw) {
        Ok(state) => Ok(state),
        Err(e) => {
            let backup = path.with_extension("json.bak");
            let _ = fs::write(&backup, raw);
            write_reflect_runtime_log(
                path.parent().unwrap_or_else(|| Path::new(".")),
                &format!("WARN reset corrupt goal_state {}: {e}", path.display()),
            )?;
            Ok(serde_json::json!({"status":"done_corrupt","error":e.to_string()}))
        }
    }
}

fn atomic_write_json(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("json")
    ));
    fs::write(&tmp, bytes).with_context(|| format!("write temp {}", tmp.display()))?;
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("replace {} with {}", path.display(), tmp.display()))?;
    Ok(())
}

fn reflect_check_scheduler(root_dir: &Path, now: NaiveDateTime) -> Result<ReflectProbe> {
    let tasks_dir = root_dir.join("sche_tasks");
    let done_dir = tasks_dir.join("done");
    fs::create_dir_all(&done_dir)?;
    let mut interval = 120;
    let config_path = tasks_dir.join("_scheduler.json");
    if let Ok(config) = fs::read_to_string(&config_path)
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&config)
        && let Some(v) = value.get("interval").and_then(serde_json::Value::as_u64)
    {
        interval = v;
    }
    let Ok(rd) = fs::read_dir(&tasks_dir) else {
        return Ok(ReflectProbe {
            task: None,
            interval,
            once: false,
        });
    };
    let done_files = fs::read_dir(&done_dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut task_files = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.ends_with(".json") && !s.starts_with('_'))
        })
        .collect::<Vec<_>>();
    task_files.sort();
    for path in task_files {
        let tid = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("task")
            .to_string();
        let value: serde_json::Value = match serde_json::from_str(&fs::read_to_string(&path)?) {
            Ok(value) => value,
            Err(e) => {
                write_scheduler_log(root_dir, &format!("ERROR parse {tid}: {e}"))?;
                continue;
            }
        };
        if value.get("enabled").and_then(serde_json::Value::as_bool) != Some(true) {
            continue;
        }
        let repeat = value
            .get("repeat")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("daily");
        let schedule = value
            .get("schedule")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("00:00");
        let Some((hour, minute)) = parse_hhmm(schedule) else {
            write_scheduler_log(
                root_dir,
                &format!("ERROR invalid schedule {tid}: {schedule}"),
            )?;
            continue;
        };
        if repeat == "weekday" && now.weekday().number_from_monday() >= 6 {
            continue;
        }
        let now_minutes = now.hour() as i64 * 60 + now.minute() as i64;
        let sched_minutes = hour as i64 * 60 + minute as i64;
        if now_minutes < sched_minutes {
            continue;
        }
        let max_delay_hours = value
            .get("max_delay_hours")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(6.0);
        if (now_minutes - sched_minutes) as f64 > max_delay_hours * 60.0 {
            write_scheduler_log(
                root_dir,
                &format!(
                    "SKIP {tid}: {}min past schedule, exceeds max_delay={max_delay_hours}h",
                    now_minutes - sched_minutes
                ),
            )?;
            continue;
        }
        if let Some(last) = last_scheduler_run(&tid, &done_files)
            && now - last < scheduler_cooldown(repeat)
        {
            continue;
        }
        let ts = now.format("%Y-%m-%d_%H%M").to_string();
        let report_path = done_dir.join(format!("{ts}_{tid}.md"));
        let prompt = value
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        write_scheduler_log(
            root_dir,
            &format!("TRIGGER {tid} (repeat={repeat}, schedule={schedule})"),
        )?;
        return Ok(ReflectProbe {
            task: Some(format!(
                "[定时任务] {tid}\n[报告路径] {}\n\n先读 scheduled_task_sop 了解执行流程，然后执行以下任务：\n\n{prompt}\n\n完成后将执行报告写入 {}。",
                report_path.display(),
                report_path.display()
            )),
            interval,
            once: false,
        });
    }
    Ok(ReflectProbe {
        task: None,
        interval,
        once: false,
    })
}

fn parse_hhmm(raw: &str) -> Option<(u32, u32)> {
    let (h, m) = raw.split_once(':')?;
    let h = h.parse::<u32>().ok()?;
    let m = m.parse::<u32>().ok()?;
    (h < 24 && m < 60).then_some((h, m))
}

fn scheduler_cooldown(repeat: &str) -> chrono::Duration {
    match repeat {
        "once" => chrono::Duration::days(999_999),
        "daily" | "weekday" => chrono::Duration::hours(20),
        "weekly" => chrono::Duration::days(6),
        "monthly" => chrono::Duration::days(27),
        _ if repeat.starts_with("every_") => {
            let spec = repeat.trim_start_matches("every_");
            let (num, unit) = spec.split_at(spec.len().saturating_sub(1));
            let n = num.parse::<i64>().unwrap_or(20);
            match unit {
                "h" => chrono::Duration::hours(n),
                "m" => chrono::Duration::minutes(n),
                "d" => chrono::Duration::days(n),
                _ => chrono::Duration::hours(20),
            }
        }
        _ => chrono::Duration::hours(20),
    }
}

fn last_scheduler_run(tid: &str, done_files: &[String]) -> Option<NaiveDateTime> {
    done_files
        .iter()
        .filter_map(|name| {
            if !name.ends_with(&format!("_{tid}.md")) {
                return None;
            }
            let stamp = name.get(..15)?;
            NaiveDateTime::parse_from_str(&format!("{stamp}:00"), "%Y-%m-%d_%H%M:%S").ok()
        })
        .max()
}

fn write_scheduler_log(root_dir: &Path, line: &str) -> Result<()> {
    let dir = root_dir.join("sche_tasks");
    fs::create_dir_all(&dir)?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("scheduler.log"))?;
    writeln!(
        f,
        "{} {line}",
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    )?;
    Ok(())
}

fn write_reflect_log(cfg: &AgentConfig, script_name: &str, result: &str) -> Result<()> {
    let dir = cfg.temp_dir.join("reflect_logs");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "{}_{}.log",
        script_name,
        chrono::Local::now().format("%Y-%m-%d")
    ));
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "[{}]", chrono::Local::now().format("%m-%d %H:%M"))?;
    writeln!(f, "{result}\n")?;
    Ok(())
}

fn build_runtime(cfg: AgentConfig) -> Result<AgentRuntime> {
    let llm = OpenAiClient::multi_arc(cfg.clone());
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    AgentRuntime::new(cfg, llm, tools)
}

fn env_flag_enabled(name: &str) -> bool {
    env::var(name)
        .ok()
        .is_some_and(|value| matches_env_truthy(&value))
}

fn matches_env_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "full"
    )
}

async fn run_tui(cfg: AgentConfig) -> Result<()> {
    let mut sessions = BTreeMap::new();
    let runtime = build_runtime(cfg.clone())?;
    sessions.insert(1usize, TuiSession::new(1, "main".into(), runtime));
    let mut active = 1usize;
    let mut next_id = 2usize;
    let mut input_history = InputHistory::default();
    draw_tui_header(&sessions, active)?;
    loop {
        let prompt_name = sessions
            .get(&active)
            .map(|s| format!("#{} {}", s.id, s.name))
            .unwrap_or_else(|| "#?".into());
        print!("\n{prompt_name} > ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let line = line.trim_end();
        if line == "/exit" || line == "/quit" {
            break;
        }
        if line.is_empty() {
            continue;
        }
        if line == "/prev" {
            println!("{}", input_history.previous().unwrap_or(""));
            continue;
        }
        if line == "/next" {
            println!("{}", input_history.next().unwrap_or(""));
            continue;
        }
        if handle_tui_command(line, &cfg, &mut sessions, &mut active, &mut next_id).await? {
            continue;
        }
        input_history.push(line);
        let session = sessions.get_mut(&active).expect("active session exists");
        session.transcript.push(("user".into(), line.to_string()));
        session.status = "running".into();
        print_agent_prefix()?;
        let fold = session.fold;
        let live_output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let captured_output = std::sync::Arc::clone(&live_output);
        match session
            .runtime
            .put_task_with_events(line.to_string(), |event| {
                if let Some(chunk) = render_tui_event(&event, fold) {
                    print!("{chunk}");
                    let _ = io::stdout().flush();
                    captured_output.lock().unwrap().push_str(&chunk);
                }
            })
            .await
        {
            Ok(out) => {
                let live_output = live_output.lock().unwrap().clone();
                let rendered = if live_output.trim().is_empty() {
                    render_agent_output(&out)
                } else {
                    live_output.trim_end().to_string()
                };
                if live_output.trim().is_empty() {
                    println!("{rendered}");
                } else {
                    println!();
                }
                let session = sessions.get_mut(&active).expect("active session exists");
                session.transcript.push(("assistant".into(), rendered));
                session.status = "idle".into();
            }
            Err(e) => {
                let session = sessions.get_mut(&active).expect("active session exists");
                session.status = "error".into();
                eprintln!("error: {e:#}");
            }
        }
    }
    Ok(())
}

struct TuiSession {
    id: usize,
    name: String,
    status: String,
    runtime: AgentRuntime,
    transcript: Vec<(String, String)>,
    fold: bool,
}

impl TuiSession {
    fn new(id: usize, name: String, runtime: AgentRuntime) -> Self {
        Self {
            id,
            name,
            status: "idle".into(),
            runtime,
            transcript: Vec::new(),
            fold: true,
        }
    }
}

#[derive(Debug, Default)]
struct InputHistory {
    items: Vec<String>,
    cursor: Option<usize>,
}

impl InputHistory {
    fn push(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if self.items.last().is_none_or(|last| last != line) {
            self.items.push(line.to_string());
        }
        self.cursor = None;
    }

    fn previous(&mut self) -> Option<&str> {
        if self.items.is_empty() {
            return None;
        }
        let idx = self
            .cursor
            .map(|idx| idx.saturating_sub(1))
            .unwrap_or_else(|| self.items.len() - 1);
        self.cursor = Some(idx);
        self.items.get(idx).map(String::as_str)
    }

    fn next(&mut self) -> Option<&str> {
        let cursor = self.cursor?;
        if cursor + 1 >= self.items.len() {
            self.cursor = None;
            Some("")
        } else {
            self.cursor = Some(cursor + 1);
            self.items.get(cursor + 1).map(String::as_str)
        }
    }
}

async fn handle_tui_command(
    line: &str,
    cfg: &AgentConfig,
    sessions: &mut BTreeMap<usize, TuiSession>,
    active: &mut usize,
    next_id: &mut usize,
) -> Result<bool> {
    if !line.starts_with('/') {
        return Ok(false);
    }
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or_default();
    let args = parts.collect::<Vec<_>>();
    match cmd {
        "/help" => {
            println!("{}", tui_help_text());
            Ok(true)
        }
        "/clear" => {
            if let Some(session) = sessions.get_mut(active) {
                session.transcript.clear();
                draw_tui_header(sessions, *active)?;
            }
            Ok(true)
        }
        "/history" => {
            if let Some(session) = sessions.get(active) {
                render_history(&session.transcript)?;
            }
            Ok(true)
        }
        "/tail" => {
            let n = args
                .first()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(6);
            if let Some(session) = sessions.get(active) {
                render_history_tail(&session.transcript, n)?;
            }
            Ok(true)
        }
        "/view" => {
            let start = args
                .first()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1)
                .saturating_sub(1);
            let count = args
                .get(1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(8);
            if let Some(session) = sessions.get(active) {
                render_history_window(&session.transcript, start, count)?;
            }
            Ok(true)
        }
        "/search" => {
            let query = args.join(" ");
            if query.trim().is_empty() {
                println!("usage: /search <keyword>");
            } else if let Some(session) = sessions.get(active) {
                render_history_search(&session.transcript, &query)?;
            }
            Ok(true)
        }
        "/save" => {
            let path = args.first().context("usage: /save <file>")?;
            let session = sessions.get(active).expect("active session exists");
            save_transcript(path, &session.transcript)?;
            println!("saved: {path}");
            Ok(true)
        }
        "/rename" => {
            let name = args.join(" ");
            if name.trim().is_empty() {
                println!("usage: /rename <name>");
            } else if let Some(session) = sessions.get_mut(active) {
                session.name = trim_chars(name.trim(), 40);
                draw_tui_header(sessions, *active)?;
            }
            Ok(true)
        }
        "/panel" | "/redraw" => {
            draw_tui_header(sessions, *active)?;
            Ok(true)
        }
        "/sessions" => {
            render_sessions(sessions, *active);
            Ok(true)
        }
        "/new" => {
            let name = if args.is_empty() {
                format!("agent-{next_id}")
            } else {
                args.join(" ")
            };
            let id = *next_id;
            *next_id += 1;
            sessions.insert(id, TuiSession::new(id, name, build_runtime(cfg.clone())?));
            *active = id;
            println!("created and switched to session #{id}");
            Ok(true)
        }
        "/branch" => {
            let Some(old) = sessions.get(active) else {
                return Ok(true);
            };
            let name = if args.is_empty() {
                format!("{}-branch", old.name)
            } else {
                args.join(" ")
            };
            let id = *next_id;
            *next_id += 1;
            let mut branched = TuiSession::new(id, name, old.runtime.fork_session());
            branched.transcript = old.transcript.clone();
            sessions.insert(id, branched);
            *active = id;
            println!("branched and switched to session #{id}");
            Ok(true)
        }
        "/switch" => {
            let key = args.first().context("usage: /switch <id|name>")?;
            let target = key
                .parse::<usize>()
                .ok()
                .filter(|id| sessions.contains_key(id))
                .or_else(|| {
                    sessions
                        .iter()
                        .find_map(|(id, s)| (s.name == *key).then_some(*id))
                });
            if let Some(id) = target {
                *active = id;
                println!("switched to session #{id}");
            } else {
                println!("no session found for {key}");
            }
            Ok(true)
        }
        "/close" => {
            if sessions.len() <= 1 {
                println!("cannot close the last session");
            } else {
                let closed = *active;
                sessions.remove(&closed);
                *active = *sessions
                    .keys()
                    .next()
                    .expect("at least one session remains");
                println!("closed session #{closed}; switched to #{active}");
            }
            Ok(true)
        }
        "/rewind" => {
            let n = args
                .first()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1);
            let session = sessions.get(active).expect("active session exists");
            match session.runtime.rewind_user_turns(n) {
                Ok(removed) => println!("rewound {n} user turn(s), removed {removed} LLM messages"),
                Err(e) => println!("rewind failed: {e:#}"),
            }
            Ok(true)
        }
        "/fold" => {
            if let Some(session) = sessions.get_mut(active) {
                session.fold = !session.fold;
                println!(
                    "display fold mode: {}",
                    if session.fold { "on" } else { "off" }
                );
            }
            Ok(true)
        }
        // Runtime slash commands keep parity with GenericAgent and are displayed in the transcript.
        "/status" | "/llm" | "/llms" | "/stop" | "/continue" | "/resume" | "/btw" | "/newctx" => {
            Ok(false)
        }
        _ if line.starts_with("/continue ") || line.starts_with("/btw ") => Ok(false),
        _ => Ok(false),
    }
}

fn draw_tui_header(sessions: &BTreeMap<usize, TuiSession>, active: usize) -> Result<()> {
    execute!(
        io::stdout(),
        Clear(ClearType::All),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print("Koda Agent TUI\n"),
        ResetColor,
        SetAttribute(Attribute::Reset),
        Print(format_tui_header(sessions, active)),
    )?;
    Ok(())
}

fn format_tui_header(sessions: &BTreeMap<usize, TuiSession>, active: usize) -> String {
    let Some(session) = sessions.get(&active) else {
        return "No active session\n".into();
    };
    let llm = session
        .runtime
        .list_llms()
        .into_iter()
        .find(|(_, _, cur)| *cur)
        .map(|(_, n, _)| n)
        .unwrap_or_else(|| "unknown".into());
    let total_msgs: usize = sessions.values().map(|s| s.transcript.len()).sum();
    let mut out = String::new();
    out.push_str(
        "┌─ Commands ─────────────────────────────────────────────────────────────────┐\n",
    );
    out.push_str("│ /help /sessions /new /branch /switch /rename /rewind /fold /tail /view    │\n");
    out.push_str("│ /search /prev /next /status /llms /btw <q> /save /clear /panel /exit      │\n");
    out.push_str(
        "└────────────────────────────────────────────────────────────────────────────┘\n",
    );
    out.push_str(
        "┌─ Sessions ─────────────────────────────────────────────────────────────────┐\n",
    );
    for (id, s) in sessions {
        let mark = if *id == active { ">" } else { " " };
        let last = tui_session_last_user(s);
        out.push_str(&format_tui_row(&format!(
            "{mark} #{:<2} {:<18} {:<7} msgs={:<3} llm={:<3} {}",
            id,
            trim_chars(&s.name, 18),
            s.status,
            s.transcript.len(),
            s.runtime.message_count(),
            trim_chars(&last, 48)
        )));
    }
    out.push_str(
        "└────────────────────────────────────────────────────────────────────────────┘\n",
    );
    out.push_str(&format_tui_row(&format!(
        "Active: #{} | sessions={} transcript_msgs={} | {} [{}] fold={}",
        session.id,
        sessions.len(),
        total_msgs,
        trim_chars(&session.name, 24),
        session.status,
        session.fold,
    )));
    out.push_str(&format_tui_row(&format!(
        "LLM: {} | tips: /tail 8, /view 1 8, /search keyword, /panel redraw",
        llm
    )));
    out
}

fn format_tui_row(text: &str) -> String {
    format!("│ {:<74} │\n", trim_chars(text, 74))
}

fn tui_help_text() -> &'static str {
    "Commands:\n\
     /help - show this help\n\
     /new [name] - create and switch to a new isolated runtime session\n\
     /branch [name] - fork current runtime history and display transcript\n\
     /switch <id|name> - switch active session\n\
     /sessions - list sessions\n\
     /rename <name> - rename current session\n\
     /rewind [n] - truncate the latest n user turns from runtime history\n\
     /fold - toggle compact event rendering\n\
     /prev, /next - browse submitted input history in line-mode terminals\n\
     /status, /llm, /llms, /stop, /continue, /btw - pass through to GenericAgent runtime\n\
     /history - render current display transcript\n\
     /tail [n] - render the latest n transcript messages\n\
     /view <start> [n] - render n transcript messages from 1-based start\n\
     /search <keyword> - search current transcript and show matching messages\n\
     /save <file> - save current transcript as markdown\n\
     /clear - clear current display transcript\n\
     /panel - redraw the session panel\n\
     /close - close current session\n\
     /quit - exit TUI"
}

fn render_sessions(sessions: &BTreeMap<usize, TuiSession>, active: usize) {
    println!("Sessions:");
    for (id, session) in sessions {
        let mark = if *id == active { "*" } else { " " };
        let last_user = tui_session_last_user(session);
        println!(
            "{mark} #{id} {} [{}] messages={} llm_messages={} {}",
            session.name,
            session.status,
            session.transcript.len(),
            session.runtime.message_count(),
            last_user.chars().take(60).collect::<String>()
        );
    }
}

fn tui_session_last_user(session: &TuiSession) -> String {
    session
        .transcript
        .iter()
        .rev()
        .find_map(|(role, text)| (role == "user").then(|| text.replace('\n', " ")))
        .unwrap_or_default()
}

fn trim_chars(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        text.to_string()
    } else {
        format!(
            "{}...",
            text.chars().take(max.saturating_sub(3)).collect::<String>()
        )
    }
}

fn render_tui_event(event: &AgentEvent, fold: bool) -> Option<String> {
    match event {
        AgentEvent::SlashOutput { content } => Some(format!("{content}\n")),
        AgentEvent::TurnStarted { turn } => Some(format!("\n── LLM Running (Turn {turn}) ──\n")),
        AgentEvent::AssistantMessage { content, .. } => {
            let text = if fold {
                fold_agent_text(content)
            } else {
                content.clone()
            };
            Some(format!("{text}\n"))
        }
        AgentEvent::AssistantMessageDelta { content, .. } => Some(content.clone()),
        AgentEvent::ThinkingMessage { content, .. } => {
            Some(format!("💭 {}\n", fold_agent_text(content)))
        }
        AgentEvent::ThinkingMessageDelta { .. } => None,
        AgentEvent::ToolStarted { name, args, .. } => {
            Some(format!("🔧 Tool `{name}` args: {args}\n"))
        }
        AgentEvent::ToolFinished { data, .. } => {
            let text = if fold {
                fold_agent_text(&data.to_string())
            } else {
                data.to_string()
            };
            Some(format!("{text}\n"))
        }
        AgentEvent::TurnFinished { stop_reason, .. } => Some(format!("[done: {stop_reason}]\n")),
        AgentEvent::LlmUsage { usage, .. } => Some(format!(
            "[usage: input={} output={} total={} cached={}]\n",
            usage.input_tokens.unwrap_or_default(),
            usage.output_tokens.unwrap_or_default(),
            usage.total_tokens.unwrap_or_default(),
            usage.cached_tokens.unwrap_or_default()
        )),
        AgentEvent::Stopped => Some("[stopped]\n".into()),
    }
}

fn fold_agent_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.chars().count() > 240 {
                format!(
                    "{} ...",
                    line.chars().take(240).collect::<String>().trim_end()
                )
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_agent_prefix() -> Result<()> {
    execute!(
        io::stdout(),
        SetForegroundColor(Color::Green),
        SetAttribute(Attribute::Bold),
        Print("Agent > "),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn render_agent_output(out: &str) -> String {
    out.lines()
        .map(|line| {
            if line.starts_with("**LLM Running") {
                format!("── {}", line.trim_matches('*'))
            } else if line.starts_with("🛠️ Tool:") {
                format!("🔧 {line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_history(transcript: &[(String, String)]) -> Result<()> {
    render_history_slice(transcript, 0)
}

fn render_history_tail(transcript: &[(String, String)], n: usize) -> Result<()> {
    let start = transcript.len().saturating_sub(n.max(1));
    render_history_slice(transcript, start)
}

fn render_history_window(
    transcript: &[(String, String)],
    start: usize,
    count: usize,
) -> Result<()> {
    if transcript.is_empty() {
        println!("(empty history)");
        return Ok(());
    }
    let indexes = history_window_indexes(transcript.len(), start, count);
    if indexes.is_empty() {
        println!("(no messages in requested window)");
        return Ok(());
    }
    render_history_indexes(transcript, &indexes)
}

fn render_history_search(transcript: &[(String, String)], query: &str) -> Result<()> {
    let indexes = search_transcript_indexes(transcript, query, 20);
    if indexes.is_empty() {
        println!("(no transcript matches for {query:?})");
        return Ok(());
    }
    render_history_indexes(transcript, &indexes)
}

fn render_history_slice(transcript: &[(String, String)], start: usize) -> Result<()> {
    if transcript.is_empty() {
        println!("(empty history)");
        return Ok(());
    }
    let indexes = (start..transcript.len()).collect::<Vec<_>>();
    render_history_indexes(transcript, &indexes)
}

fn render_history_indexes(transcript: &[(String, String)], indexes: &[usize]) -> Result<()> {
    for idx in indexes {
        let Some((role, text)) = transcript.get(*idx) else {
            continue;
        };
        let color = if role == "user" {
            Color::Yellow
        } else {
            Color::Green
        };
        execute!(
            io::stdout(),
            SetForegroundColor(color),
            SetAttribute(Attribute::Bold),
            Print(format!("\n[{}] {}\n", idx + 1, role)),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Print(text),
            Print("\n")
        )?;
    }
    Ok(())
}

fn history_window_indexes(len: usize, start: usize, count: usize) -> Vec<usize> {
    let count = count.max(1);
    (start.min(len)..len.min(start.saturating_add(count))).collect()
}

fn search_transcript_indexes(
    transcript: &[(String, String)],
    query: &str,
    limit: usize,
) -> Vec<usize> {
    let query = query.to_ascii_lowercase();
    transcript
        .iter()
        .enumerate()
        .filter_map(|(idx, (role, text))| {
            let haystack = format!("{role}\n{text}").to_ascii_lowercase();
            haystack.contains(&query).then_some(idx)
        })
        .take(limit.max(1))
        .collect()
}

fn save_transcript(path: &str, transcript: &[(String, String)]) -> Result<()> {
    let content = transcript
        .iter()
        .map(|(role, text)| format!("## {role}\n\n{text}\n"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, content)?;
    Ok(())
}

async fn run_task_mode(
    runtime: AgentRuntime,
    cfg: AgentConfig,
    task: String,
    input: Option<String>,
) -> Result<()> {
    let dir = task_dir(&cfg, &task);
    fs::create_dir_all(&dir)?;
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("output"))
        {
            let _ = fs::remove_file(path);
        }
    }
    let infile = dir.join("input.txt");
    if let Some(input) = input {
        fs::write(&infile, input)?;
    }
    if let Some(history) = consume_file(&dir, "_history.json")? {
        let messages = serde_json::from_str::<Vec<koda_agent_core::ChatMessage>>(&history)
            .or_else(|_| {
                serde_json::from_str::<Vec<serde_json::Value>>(&history).map(|values| {
                    values
                        .into_iter()
                        .filter_map(|v| serde_json::from_value(v).ok())
                        .collect::<Vec<_>>()
                })
            })
            .context("parse task _history.json")?;
        runtime.restore_messages(messages);
    }
    let mut raw =
        fs::read_to_string(&infile).with_context(|| format!("read {}", infile.display()))?;
    let mut round = String::new();
    loop {
        if dir.join("_stop").exists() {
            runtime.abort();
        }
        let output_path = dir.join(format!("output{round}.txt"));
        let latest = Arc::new(Mutex::new(String::new()));
        let latest_for_events = Arc::clone(&latest);
        let output_for_events = output_path.clone();
        let out = runtime
            .put_task_with_events(raw.clone(), move |event| {
                let mut latest = latest_for_events.lock().expect("task output lock");
                match event {
                    AgentEvent::AssistantMessage { content, .. } if !content.trim().is_empty() => {
                        latest.push_str(&content);
                        latest.push('\n');
                    }
                    AgentEvent::ToolStarted { name, args, .. } => {
                        latest.push_str(&format!("\n[Tool] {name} {args}\n"));
                    }
                    AgentEvent::ToolFinished { name, data, .. } => {
                        latest.push_str(&format!("[ToolResult] {name} {data}\n"));
                    }
                    _ => {}
                }
                if !latest.is_empty() {
                    let _ = fs::write(&output_for_events, latest.as_bytes());
                }
            })
            .await?;
        fs::write(&output_path, format!("{out}\n\n[ROUND END]\n"))?;
        let _ = fs::remove_file(dir.join("_stop"));
        let reply = wait_reply(&dir).await?;
        let Some(reply) = reply else {
            break;
        };
        fs::write(&infile, &reply)?;
        raw = reply;
        round = if round.is_empty() {
            "1".into()
        } else {
            (round.parse::<usize>().unwrap_or(1) + 1).to_string()
        };
    }
    Ok(())
}

fn task_dir(cfg: &AgentConfig, task: &str) -> PathBuf {
    let path = Path::new(task);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cfg.temp_dir.join(path)
    }
}

fn consume_file(dir: &Path, name: &str) -> Result<Option<String>> {
    let path = dir.join(name);
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)?;
    fs::remove_file(path)?;
    Ok(Some(content))
}

async fn wait_reply(dir: &Path) -> Result<Option<String>> {
    for _ in 0..300 {
        let reply = dir.join("reply.txt");
        if reply.exists() {
            let content = fs::read_to_string(&reply)?;
            fs::remove_file(reply)?;
            return Ok(Some(content));
        }
        sleep(Duration::from_secs(2)).await;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_agent_core::AgentResponse;
    use koda_agent_llm::MockLlmClient;

    fn test_agent_config(root: &Path) -> AgentConfig {
        AgentConfig {
            home_dir: root.into(),
            workspace_dir: root.into(),
            resource_dir: root.into(),
            root_dir: root.into(),
            temp_dir: root.join("temp"),
            memory_dir: root.join("memory"),
            logs_dir: root.join("logs"),
            sessions_dir: root.join("sessions"),
            browser_dir: root.join("browser"),
            openai_base_url: "http://localhost".into(),
            openai_api_key: "sk-test".into(),
            openai_model: "mock".into(),
            llm_api_style: "chat".into(),
            max_turns: 4,
            verbose: false,
            stream: false,
            timeout_secs: 600,
            connect_timeout_secs: 30,
            verify_tls: true,
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            thinking_type: None,
            thinking_budget_tokens: None,
            service_tier: None,
            proxy: None,
            failover: true,
            custom_headers: Default::default(),
            mixin: Default::default(),
            llm_configs: vec![],
        }
    }

    fn test_tui_session(root: &Path, id: usize, name: &str) -> TuiSession {
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::create_dir_all(root.join("temp")).unwrap();
        fs::write(root.join("assets/tools_schema.json"), "[]").unwrap();
        fs::write(root.join("assets/sys_prompt.txt"), "You are GenericAgent.").unwrap();
        let cfg = test_agent_config(root);
        let llm = Arc::new(MockLlmClient {
            responses: Arc::new(vec![AgentResponse {
                thinking: String::new(),
                content: "ok".into(),
                tool_calls: vec![],
                raw: serde_json::Value::Null,
            }]),
        });
        let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
        TuiSession::new(
            id,
            name.to_string(),
            AgentRuntime::new(cfg, llm, tools).unwrap(),
        )
    }

    #[test]
    fn reflect_probe_parser_accepts_task_interval_once() {
        let probe =
            parse_reflect_probe_output(r#"{"ok":true,"task":"do it","interval":1,"once":true}"#)
                .unwrap();
        assert_eq!(probe.task.as_deref(), Some("do it"));
        assert_eq!(probe.interval, 1);
        assert!(probe.once);
    }

    #[test]
    fn reflect_probe_parser_reports_script_error() {
        let err = parse_reflect_probe_output(r#"{"ok":false,"error":"boom"}"#).unwrap_err();
        assert!(format!("{err:#}").contains("boom"));
    }

    #[test]
    fn tui_header_renders_session_sidebar_like_upstream_panel() {
        let d = tempfile::tempdir().unwrap();
        let mut sessions = BTreeMap::new();
        let mut main = test_tui_session(d.path(), 1, "main");
        main.transcript
            .push(("user".into(), "first question".into()));
        sessions.insert(1, main);
        let mut branch = test_tui_session(d.path(), 2, "analysis-branch-with-long-name");
        branch.status = "running".into();
        branch.transcript.push((
            "user".into(),
            "second question with a fairly long preview that should be shortened".into(),
        ));
        sessions.insert(2, branch);

        let header = format_tui_header(&sessions, 2);
        assert!(header.contains("Sessions"));
        assert!(header.contains("/rename"));
        assert!(header.contains("/tail"));
        assert!(header.contains("/search"));
        assert!(header.contains("> #2"));
        assert!(header.contains("analysis-branch"));
        assert!(header.contains("second question"));
        assert!(header.contains("Active: #2"));
        assert!(header.contains("transcript_msgs=2"));
        assert!(header.contains("LLM: MockLLM"));
    }

    #[test]
    fn tui_scrollback_window_and_search_are_stable() {
        let transcript = vec![
            ("user".into(), "alpha question".into()),
            ("assistant".into(), "beta answer".into()),
            ("user".into(), "gamma followup".into()),
            ("assistant".into(), "delta answer with Alpha detail".into()),
        ];
        assert_eq!(history_window_indexes(transcript.len(), 1, 2), vec![1, 2]);
        assert_eq!(
            history_window_indexes(transcript.len(), 99, 5),
            Vec::<usize>::new()
        );
        assert_eq!(history_window_indexes(transcript.len(), 0, 0), vec![0]);
        assert_eq!(
            search_transcript_indexes(&transcript, "alpha", 10),
            vec![0, 3]
        );
        assert_eq!(
            search_transcript_indexes(&transcript, "assistant", 10),
            vec![1, 3]
        );
        assert_eq!(
            search_transcript_indexes(&transcript, "missing", 10),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn tui_input_history_tracks_unique_lines_and_cursor() {
        let mut history = InputHistory::default();
        history.push("hello");
        history.push("hello");
        history.push("world");
        assert_eq!(history.items, vec!["hello", "world"]);
        assert_eq!(history.previous(), Some("world"));
        assert_eq!(history.previous(), Some("hello"));
        assert_eq!(history.previous(), Some("hello"));
        assert_eq!(history.next(), Some("world"));
        assert_eq!(history.next(), Some(""));
        assert_eq!(history.next(), None);
    }

    #[test]
    fn tui_full_env_flag_parser_accepts_only_explicit_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "on", "full", " Full "] {
            assert!(matches_env_truthy(value), "{value}");
        }
        for value in ["", "0", "false", "no", "off", "line", "maybe"] {
            assert!(!matches_env_truthy(value), "{value}");
        }
    }

    #[test]
    fn json_reflect_rule_supports_no_python_mode() {
        let d = tempfile::tempdir().unwrap();
        let rule = d.path().join("watch.json");
        fs::write(
            &rule,
            r#"{"task":"hello","interval":2,"once":true,"watch_file":"flag","trigger":"exists"}"#,
        )
        .unwrap();
        let probe = reflect_check_json(&rule, d.path()).unwrap();
        assert!(probe.task.is_none());
        fs::write(d.path().join("flag"), "1").unwrap();
        let probe = reflect_check_json(&rule, d.path()).unwrap();
        assert_eq!(probe.task.as_deref(), Some("hello"));
        assert_eq!(probe.interval, 2);
        assert!(probe.once);
    }

    #[test]
    fn native_scheduler_reflect_triggers_due_task_and_respects_cooldown() {
        let d = tempfile::tempdir().unwrap();
        let tasks = d.path().join("sche_tasks");
        fs::create_dir_all(&tasks).unwrap();
        fs::write(
            tasks.join("daily_report.json"),
            r#"{"enabled":true,"repeat":"daily","schedule":"09:00","prompt":"write report","max_delay_hours":6}"#,
        )
        .unwrap();
        let now =
            NaiveDateTime::parse_from_str("2026-05-10 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let probe = reflect_check_scheduler(d.path(), now).unwrap();
        let task = probe.task.unwrap();
        assert!(task.contains("[定时任务] daily_report"));
        assert!(task.contains("scheduled_task_sop"));
        assert!(task.contains("write report"));

        let done = tasks.join("done");
        fs::create_dir_all(&done).unwrap();
        fs::write(done.join("2026-05-10_0930_daily_report.md"), "done").unwrap();
        let probe = reflect_check_scheduler(d.path(), now).unwrap();
        assert!(probe.task.is_none());
    }

    #[test]
    fn native_scheduler_reflect_skips_late_window_and_weekend_weekday() {
        let d = tempfile::tempdir().unwrap();
        let tasks = d.path().join("sche_tasks");
        fs::create_dir_all(&tasks).unwrap();
        fs::write(
            tasks.join("late.json"),
            r#"{"enabled":true,"repeat":"daily","schedule":"01:00","prompt":"late","max_delay_hours":1}"#,
        )
        .unwrap();
        let now =
            NaiveDateTime::parse_from_str("2026-05-10 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        assert!(
            reflect_check_scheduler(d.path(), now)
                .unwrap()
                .task
                .is_none()
        );

        fs::write(
            tasks.join("late.json"),
            r#"{"enabled":true,"repeat":"weekday","schedule":"09:00","prompt":"weekday"}"#,
        )
        .unwrap();
        assert!(
            reflect_check_scheduler(d.path(), now)
                .unwrap()
                .task
                .is_none()
        );
    }

    #[test]
    fn native_goal_mode_continues_until_budget_then_marks_done_on_done() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("goal_state.json");
        fs::write(
            &state,
            r#"{"objective":"Improve parity","status":"running","start_time":1000,"budget_seconds":600,"turns_used":0,"max_turns":2}"#,
        )
        .unwrap();

        let probe = reflect_check_goal_state(&state, 1060).unwrap();
        let task = probe.task.unwrap();
        assert!(task.contains("[Goal Mode"));
        assert!(task.contains("Improve parity"));
        assert!(task.contains("第 1 次唤醒"));
        let state_value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(state_value["turns_used"], 1);
        assert_eq!(probe.interval, 3);

        let probe = reflect_check_goal_state(&state, 2000).unwrap();
        let task = probe.task.unwrap();
        assert!(task.contains("预算耗尽"));
        let state_value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(state_value["status"], "wrapping_up");

        reflect_goal_on_done(&state, 2010).unwrap();
        let state_value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(state_value["status"], "done_budget");
        assert_eq!(state_value["end_time"], 2010);

        let probe = reflect_check_goal_state(&state, 2020).unwrap();
        assert_eq!(probe.task.as_deref(), Some("/exit"));
    }

    #[test]
    fn native_goal_mode_exits_when_state_missing() {
        let d = tempfile::tempdir().unwrap();
        let probe = reflect_check_goal_state(&d.path().join("missing.json"), 1000).unwrap();
        assert_eq!(probe.task.as_deref(), Some("/exit"));
        assert_eq!(probe.interval, 3);
    }

    #[test]
    fn native_goal_mode_corrupt_state_is_backed_up_and_exits() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("temp/goal_state.json");
        fs::create_dir_all(state.parent().unwrap()).unwrap();
        fs::write(&state, "{bad json").unwrap();

        let probe = reflect_check_goal_state(&state, 1000).unwrap();
        assert_eq!(probe.task.as_deref(), Some("/exit"));
        assert!(state.with_extension("json.bak").exists());
        assert!(
            fs::read_to_string(state.parent().unwrap().join("reflect_runtime.log"))
                .unwrap()
                .contains("reset corrupt goal_state")
        );
    }

    #[test]
    fn json_goal_reflect_kind_uses_native_on_done_state_machine() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("temp/goal_state.json");
        fs::create_dir_all(state.parent().unwrap()).unwrap();
        fs::write(
            &state,
            r#"{"objective":"Ship parity","status":"running","start_time":1000,"budget_seconds":1,"turns_used":0,"max_turns":10}"#,
        )
        .unwrap();
        let rule = d.path().join("goal.json");
        fs::write(&rule, r#"{"kind":"goal_mode"}"#).unwrap();
        let probe = reflect_check_json(&rule, d.path()).unwrap();
        assert!(probe.task.unwrap().contains("预算耗尽"));
        reflect_goal_on_done(&state, 2000).unwrap();
        let state_value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(state_value["status"], "done_budget");
    }

    #[test]
    fn native_autonomous_reflect_matches_upstream_prompt() {
        let probe = reflect_check_autonomous();
        assert_eq!(probe.interval, 1800);
        assert!(!probe.once);
        assert!(probe.task.unwrap().contains("[AUTO]"));
    }

    #[test]
    fn native_agent_team_helpers_parse_config_state_and_prompt() {
        let d = tempfile::tempdir().unwrap();
        let reflect_dir = d.path().join("reflect");
        fs::create_dir_all(&reflect_dir).unwrap();
        fs::write(
            reflect_dir.join("agent_team_setting.json"),
            r#"{"base_url":"http://bbs.local","board_key":"k"}"#,
        )
        .unwrap();
        let cfg = load_agent_team_config(d.path(), &reflect_dir.join("agent_team_worker")).unwrap();
        assert_eq!(cfg.base_url, "http://bbs.local");
        assert_eq!(cfg.board_key, "k");
        assert_eq!(
            agent_team_max_post_id(&serde_json::json!([{"id": 1}, {"id": 3}, {"id": 2}])),
            Some(3)
        );
        let prompt = agent_team_prompt(&cfg);
        assert!(prompt.contains("agent worker"));
        assert!(prompt.contains("http://bbs.local"));

        reflect_agent_team_on_done(d.path(), 1234).unwrap();
        let state = load_agent_team_state(&agent_team_state_path(d.path())).unwrap();
        assert_eq!(state.last_done, 1234);
    }

    #[test]
    fn agent_team_state_corruption_is_backed_up_and_reset() {
        let d = tempfile::tempdir().unwrap();
        let path = agent_team_state_path(d.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{bad json").unwrap();

        let state = load_agent_team_state(&path).unwrap();
        assert_eq!(state.last_id, -1);
        assert_eq!(state.last_done, -1);
        assert!(path.with_extension("json.bak").exists());
        assert!(
            fs::read_to_string(path.parent().unwrap().join("reflect_runtime.log"))
                .unwrap()
                .contains("reset corrupt agent_team_worker_state")
        );
    }

    #[tokio::test]
    async fn agent_team_poll_failure_logs_and_skips_like_upstream() {
        let d = tempfile::tempdir().unwrap();
        let reflect_dir = d.path().join("reflect");
        fs::create_dir_all(&reflect_dir).unwrap();
        fs::write(
            reflect_dir.join("agent_team_setting.json"),
            r#"{"base_url":"http://127.0.0.1:9","board_key":"k"}"#,
        )
        .unwrap();

        let probe = reflect_check_agent_team(d.path(), &reflect_dir.join("agent_team_worker"))
            .await
            .unwrap();
        assert!(probe.task.is_none());
        assert!(
            fs::read_to_string(d.path().join("temp/reflect_runtime.log"))
                .unwrap()
                .contains("agent_team board poll failed")
        );
    }

    #[test]
    fn python_extra_parser_keeps_core_first() {
        assert_eq!(parse_python_extras(&[]).unwrap(), vec![PythonExtra::Core]);
        assert_eq!(
            parse_python_extras(&["ocr".into(), "automation".into(), "ocr".into()]).unwrap(),
            vec![PythonExtra::Core, PythonExtra::Ocr, PythonExtra::Automation]
        );
        assert!(parse_python_extras(&["unknown".into()]).is_err());
    }

    #[test]
    fn task_dir_uses_home_temp_for_relative_and_keeps_absolute() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_agent_config(d.path());
        assert_eq!(task_dir(&cfg, "demo"), d.path().join("temp/demo"));
        let absolute = d.path().join("external-task");
        assert_eq!(task_dir(&cfg, absolute.to_str().unwrap()), absolute);
    }

    #[test]
    fn resources_install_copies_static_assets_without_runtime_config() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("source");
        let home = d.path().join("home");
        fs::create_dir_all(source.join("assets/tmwd_cdp_bridge")).unwrap();
        fs::create_dir_all(source.join("memory")).unwrap();
        fs::write(source.join("assets/tools_schema.json"), "[]").unwrap();
        fs::write(source.join("assets/sys_prompt.txt"), "prompt").unwrap();
        fs::write(source.join("assets/simphtml_opt.js"), "opt").unwrap();
        fs::write(source.join("assets/tmwd_cdp_bridge/manifest.json"), "{}").unwrap();
        fs::write(source.join("assets/tmwd_cdp_bridge/config.js"), "secret").unwrap();
        fs::write(source.join("memory/memory_management_sop.md"), "sop").unwrap();
        fs::write(source.join("memory/global_mem.txt"), "runtime").unwrap();
        fs::write(source.join("requirements-python-core.txt"), "# core").unwrap();

        let report = install_resources(&source, &home, false, false).unwrap();
        assert_eq!(report["doctor"]["home"]["ok"], true);
        assert!(home.join("resources/assets/tools_schema.json").is_file());
        assert!(
            home.join("resources/memory/memory_management_sop.md")
                .is_file()
        );
        assert!(
            home.join("resources/requirements-python-core.txt")
                .is_file()
        );
        assert!(
            !home
                .join("resources/assets/tmwd_cdp_bridge/config.js")
                .exists()
        );
        assert!(!home.join("resources/memory/global_mem.txt").exists());
        assert!(home.join("browser/tmwd_cdp_bridge/manifest.json").is_file());
        assert!(home.join("browser/tmwd_cdp_bridge/config.js").is_file());
        assert_eq!(report["doctor"]["browser"]["installed"], true);
        assert_eq!(report["doctor"]["browser"]["runtime_config"], true);
    }

    #[test]
    fn resources_install_noops_when_source_is_home_resources() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let resources = home.join("resources");
        fs::create_dir_all(resources.join("assets")).unwrap();
        fs::write(resources.join("assets/tools_schema.json"), "[]").unwrap();
        let report = install_resources(&resources, &home, true, false).unwrap();
        assert_eq!(report["copied"].as_array().unwrap().len(), 0);
        assert!(
            report["skipped"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str().unwrap_or_default().contains("already"))
        );
    }

    #[test]
    fn update_platform_mapping_covers_release_targets() {
        let cases = [
            ("macos", "x86_64", "x86_64-apple-darwin", "tar.gz"),
            ("macos", "aarch64", "aarch64-apple-darwin", "tar.gz"),
            ("linux", "x86_64", "x86_64-unknown-linux-gnu", "tar.gz"),
            ("linux", "aarch64", "aarch64-unknown-linux-gnu", "tar.gz"),
            ("windows", "x86_64", "x86_64-pc-windows-msvc", "zip"),
            ("windows", "aarch64", "aarch64-pc-windows-msvc", "zip"),
        ];
        for (os, arch, target, suffix) in cases {
            let platform = update_platform_for(os, arch).unwrap();
            assert_eq!(platform.target, target);
            assert_eq!(
                platform.archive_name,
                format!("koda-agent-{target}.{suffix}")
            );
        }
        assert!(update_platform_for("freebsd", "x86_64").is_err());
    }

    #[test]
    fn update_release_urls_and_checksums_match_github_shape() {
        let platform = update_platform_for("linux", "x86_64").unwrap();
        let latest = release_urls("koda-claw/koda-agent", "latest", &platform);
        assert_eq!(
            latest.archive,
            "https://github.com/koda-claw/koda-agent/releases/latest/download/koda-agent-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            latest.checksums,
            "https://github.com/koda-claw/koda-agent/releases/latest/download/SHA256SUMS"
        );
        let pinned = release_urls("koda-claw/koda-agent", "v0.1.0", &platform);
        assert_eq!(
            pinned.archive,
            "https://github.com/koda-claw/koda-agent/releases/download/v0.1.0/koda-agent-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            checksum_for_archive(
                "abc  koda-agent-x86_64-unknown-linux-gnu.tar.gz\n",
                "koda-agent-x86_64-unknown-linux-gnu.tar.gz"
            )
            .as_deref(),
            Some("abc")
        );
        assert_eq!(
            checksum_for_archive(
                "abc  dist/koda-agent-x86_64-unknown-linux-gnu.tar.gz\n",
                "koda-agent-x86_64-unknown-linux-gnu.tar.gz"
            )
            .as_deref(),
            Some("abc")
        );
        assert!(validate_repo_slug("koda-claw/koda-agent").is_ok());
        assert!(validate_repo_slug("https://github.com/koda-claw/koda-agent").is_err());
    }

    #[test]
    fn update_version_check_compares_semver_like_tags() {
        use std::cmp::Ordering;

        assert_eq!(compare_version_like("0.1.1", "0.1.2"), Some(Ordering::Less));
        assert_eq!(
            compare_version_like("v0.1.2", "0.1.2"),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_version_like("0.2.0", "0.1.9"),
            Some(Ordering::Greater)
        );
        assert_eq!(parse_version_like("0.1.2-alpha.1").unwrap(), vec![0, 1, 2]);
        assert!(compare_version_like("dev", "0.1.2").is_none());
    }
}
