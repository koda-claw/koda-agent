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
    AgentConfig, AgentEvent, AgentRuntime,
    python_runtime::{
        PythonExtra, PythonPurpose, bootstrap_managed_python, doctor_python,
        python_unavailable_message, remove_managed_python, resolve_python,
    },
};
use koda_agent_frontends::{run_frontend, serve_acp_jsonl_with_factory};
use koda_agent_llm::OpenAiClient;
use koda_agent_memory::{
    archive_l4_sessions, audit_memory, cleanup_memory_indexes, init_memory, recall_l4_history,
    settle_long_term_updates, settle_long_term_updates_assisted,
};
use koda_agent_tools::GenericToolDispatcher;
use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::time::{sleep, timeout};

#[derive(Parser, Debug)]
#[command(
    name = "koda-agent",
    version,
    about = "Rust GenericAgent-compatible runtime"
)]
struct Args {
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
    if let Some(CliCommand::Doctor { json }) = &args.command {
        return run_doctor(&root, *json);
    }
    if let Some(CliCommand::BootstrapPython {
        extras,
        recreate,
        repair,
        dry_run,
        offline,
    }) = &args.command
    {
        return run_bootstrap_python(&root, extras, *recreate, *repair, *dry_run, *offline);
    }
    if let Some(CliCommand::PythonEnv { command }) = &args.command {
        return run_python_env(command);
    }
    let mut cfg = AgentConfig::from_env(root)?;
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
    let dir = cfg.temp_dir.join(task);
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
    child.current_dir(&cfg.root_dir);
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

fn run_doctor(root: &Path, json_output: bool) -> Result<()> {
    let env_path = root.join(".env");
    let python = doctor_python(root, PythonPurpose::AgentHelper);
    let llm = AgentConfig::from_env(root).ok().map(|cfg| {
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
            "workspace": root.display().to_string(),
            "env_file": env_path.exists(),
            "env_keys": {
                "OPENAI_BASE_URL": env_key_available(&env_path, "OPENAI_BASE_URL"),
                "OPENAI_API_KEY": env_key_available(&env_path, "OPENAI_API_KEY"),
                "OPENAI_MODEL": env_key_available(&env_path, "OPENAI_MODEL"),
                "OPENAI_STREAM": env_key_available(&env_path, "OPENAI_STREAM"),
            }
        },
        "llm": llm,
        "python": python,
    });
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Core");
        println!("  workspace: {}", root.display());
        println!(
            "  .env: {}",
            if env_path.exists() {
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
        return reflect_check_agent_team(&cfg.root_dir, script).await;
    }
    if is_native_goal_reflect(script) {
        return reflect_check_goal(&cfg.root_dir, chrono::Local::now().timestamp());
    }
    if is_native_scheduler_reflect(script) {
        return reflect_check_scheduler(&cfg.root_dir, chrono::Local::now().naive_local());
    }
    if script.extension().and_then(|s| s.to_str()) == Some("json") {
        return reflect_check_json(script, &cfg.root_dir);
    }
    let script = script.to_path_buf();
    let root = cfg.root_dir.clone();
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
        reflect_agent_team_on_done(&cfg.root_dir, chrono::Local::now().timestamp())?;
        return Ok(());
    }
    if is_native_goal_reflect(script)
        || matches!(
            json_reflect_kind(script).as_deref(),
            Some("goal" | "goal_mode")
        )
    {
        reflect_goal_on_done(
            &goal_state_path(&cfg.root_dir),
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
    let root = cfg.root_dir.clone();
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
    let dir = cfg.temp_dir.join(&task);
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
            root_dir: root.into(),
            temp_dir: root.join("temp"),
            memory_dir: root.join("memory"),
            logs_dir: root.join("logs"),
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
}
