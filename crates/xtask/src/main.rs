use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use koda_agent_core::{AgentConfig, AgentResponse, ToolDispatcher};
use koda_agent_memory::{
    VisionConfig, VisionRequest, ask_vision, audit_memory, cleanup_memory_indexes,
    recall_l4_history,
};
use koda_agent_tools::GenericToolDispatcher;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("browser-smoke") | Some("cdp-smoke") => browser_smoke().await,
        Some("tmwd-extension-smoke") | Some("extension-smoke") => tmwd_extension_smoke().await,
        Some("tmwd-real-matrix") | Some("tmwd-matrix") | Some("browser-matrix") => {
            tmwd_real_matrix_smoke().await
        }
        Some("tmwd-static-parity-smoke") | Some("tmwd-static-parity") => tmwd_static_parity_smoke(),
        Some("rich-monitor-smoke") => rich_monitor_smoke().await,
        Some("acp-client-smoke") | Some("acp-smoke") => acp_client_smoke().await,
        Some("vision-smoke") => vision_smoke(args.next()).await,
        Some("memory-parity-smoke") | Some("memory-smoke") => memory_parity_smoke(),
        Some("tui-smoke") | Some("tui-full-smoke") => tui_smoke(),
        Some("help") | Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some(other) => bail!("unknown xtask command: {other}"),
    }
}

fn print_help() {
    println!(
        "xtask commands:\n  browser-smoke              Verify Chrome CDP at http://127.0.0.1:9222/json\n  tmwd-extension-smoke       Verify installed tmwd_cdp_bridge through TMWebDriver master\n  tmwd-real-matrix           Verify installed bridge on a real local-page Edge/Chrome matrix\n  tmwd-static-parity-smoke   Compare bridge command surface against upstream assets\n  rich-monitor-smoke         Verify web_execute_js rich monitor on a local CDP tab\n  acp-client-smoke           Verify serve-acp with an external JSONL client process\n  vision-smoke [image]       Verify VISION_* multimodal config with a small image\n  memory-parity-smoke        Verify upstream memory files plus audit/cleanup/recall behavior\n  tui-smoke                  Verify full TUI entrypoints fail safely outside a TTY"
    );
}

fn tui_smoke() -> Result<()> {
    let root = workspace_root()?;
    let help = run_cargo_koda(&root, &["tui", "--help"], &[])?;
    ensure_output_contains("tui help", &help, "Use the stable line-mode TUI")?;

    let explicit_full = run_cargo_koda(&root, &["tui", "--full"], &[])?;
    ensure_failed_with_tty_message("explicit full TUI", &explicit_full)?;

    let env_full = run_cargo_koda(&root, &["tui"], &[("KODA_TUI_FULL", "1")])?;
    ensure_failed_with_tty_message("env full TUI", &env_full)?;

    let forced_line_help = run_cargo_koda(
        &root,
        &["tui", "--line", "--help"],
        &[("KODA_TUI_FULL", "1")],
    )?;
    ensure_output_contains(
        "forced line help",
        &forced_line_help,
        "Use the stable line-mode TUI",
    )?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "checked": [
                "tui --help exposes --line",
                "tui --full fails safely without TTY",
                "KODA_TUI_FULL=1 tui selects full TUI and fails safely without TTY",
                "tui --line remains accepted while KODA_TUI_FULL=1 is set"
            ],
            "non_tty_error": "full-screen TUI requires an interactive terminal"
        }))?
    );
    Ok(())
}

struct CmdOutput {
    status_success: bool,
    stdout: String,
    stderr: String,
}

impl CmdOutput {
    fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

fn run_cargo_koda(root: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<CmdOutput> {
    let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(root)
        .args(["run", "-q", "-p", "koda-agent-cli", "--"])
        .args(args)
        .env("TERM", "dumb");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let output = cmd.output().with_context(|| {
        format!(
            "run cargo koda command: cargo run -q -p koda-agent-cli -- {}",
            args.join(" ")
        )
    })?;
    Ok(CmdOutput {
        status_success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn ensure_output_contains(label: &str, output: &CmdOutput, needle: &str) -> Result<()> {
    let combined = output.combined();
    if !combined.contains(needle) {
        bail!("{label} missing {needle:?}; output={combined}");
    }
    Ok(())
}

fn ensure_failed_with_tty_message(label: &str, output: &CmdOutput) -> Result<()> {
    if output.status_success {
        bail!("{label} unexpectedly succeeded");
    }
    ensure_output_contains(
        label,
        output,
        "full-screen TUI requires an interactive terminal",
    )?;
    ensure_output_contains(label, output, "koda-agent tui --line")?;
    Ok(())
}

fn tmwd_static_parity_smoke() -> Result<()> {
    let root = workspace_root()?;
    let upstream = Path::new("/tmp/genericagent-inspect");
    let local_bg = fs::read_to_string(root.join("assets/tmwd_cdp_bridge/background.js"))
        .context("read local tmwd background.js")?;
    let local_content = fs::read_to_string(root.join("assets/tmwd_cdp_bridge/content.js"))
        .context("read local tmwd content.js")?;
    let local_master = fs::read_to_string(root.join("crates/koda-agent-frontends/src/lib.rs"))
        .context("read local Rust TMWebDriver master")?;
    let upstream_bg = fs::read_to_string(upstream.join("assets/tmwd_cdp_bridge/background.js"))
        .context("read upstream tmwd background.js")?;
    let upstream_content = fs::read_to_string(upstream.join("assets/tmwd_cdp_bridge/content.js"))
        .context("read upstream tmwd content.js")?;
    let upstream_master = fs::read_to_string(upstream.join("TMWebDriver.py"))
        .context("read upstream TMWebDriver.py")?;

    let base_commands = ["cookies", "cdp", "batch", "tabs"];
    let extension_commands = ["management", "contentSettings"];
    let management_methods = ["list", "reload", "disable", "enable"];
    let master_routes = ["/link", "/api/longpoll", "/api/result"];
    let master_cmds = ["get_all_sessions", "find_session", "execute_js"];

    ensure_text_has_all(
        "upstream background base commands",
        &upstream_bg,
        &base_commands,
    )?;
    ensure_text_has_all("local background base commands", &local_bg, &base_commands)?;
    ensure_text_has_all(
        "upstream background extension commands",
        &upstream_bg,
        &extension_commands,
    )?;
    ensure_text_has_all(
        "local background extension commands",
        &local_bg,
        &extension_commands,
    )?;
    ensure_text_has_all(
        "local content base commands",
        &local_content,
        &base_commands,
    )?;
    ensure_text_has_all(
        "local content extension commands",
        &local_content,
        &extension_commands,
    )?;
    ensure_text_has_all(
        "local background management methods",
        &local_bg,
        &management_methods,
    )?;
    ensure_text_has_all("upstream master routes", &upstream_master, &master_routes)?;
    ensure_text_has_all("local master routes", &local_master, &master_routes)?;
    ensure_text_has_all("upstream master commands", &upstream_master, &master_cmds)?;
    ensure_text_has_all("local master commands", &local_master, &master_cmds)?;
    ensure_text_has_all(
        "local browser fallbacks",
        &local_bg,
        &[
            "Runtime.evaluate",
            "Page.captureScreenshot",
            "scripting.executeScript",
            "tabs.captureVisibleTab",
        ],
    )?;
    ensure_text_has_all(
        "local safety guards",
        &local_bg,
        &["confirmSelf", "mayDisable", "isSelf"],
    )?;
    ensure_text_has_all(
        "upstream content base commands",
        &upstream_content,
        &base_commands,
    )?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "base_commands": base_commands,
            "extension_commands": extension_commands,
            "management_methods": management_methods,
            "master_routes": master_routes,
            "master_commands": master_cmds,
            "rust_superset_guards": ["confirmSelf", "mayDisable", "isSelf"],
            "rust_cdp_fallbacks": ["Runtime.evaluate", "Page.captureScreenshot"],
        }))?
    );
    Ok(())
}

fn ensure_text_has_all(label: &str, text: &str, needles: &[&str]) -> Result<()> {
    let missing = needles
        .iter()
        .copied()
        .filter(|needle| !text.contains(needle))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!("{label} missing markers: {}", missing.join(", "));
    }
    Ok(())
}

fn memory_parity_smoke() -> Result<()> {
    let root = workspace_root()?;
    let upstream_memory = Path::new("/tmp/genericagent-inspect/memory");
    let mut missing_upstream_files = Vec::new();
    if upstream_memory.exists() {
        for entry in fs::read_dir(upstream_memory)? {
            let entry = entry?;
            if !entry.path().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !root.join("memory").join(&name).exists() {
                missing_upstream_files.push(name);
            }
        }
        missing_upstream_files.sort();
    }

    let cfg = AgentConfig::from_env(&root)?;
    let audit = audit_memory(&cfg)?;
    let cleanup_dry = cleanup_memory_indexes(&cfg, true, true)?;
    let recall_hits = recall_l4_history(&cfg, "tmwd memory", 3)?;
    println!(
        "{}",
        json!({
            "ok": missing_upstream_files.is_empty(),
            "missing_upstream_memory_files": missing_upstream_files,
            "l1_lines": audit.l1_lines,
            "l1_invalid_lines": audit.l1_invalid_lines.len(),
            "l1_duplicate_lines": audit.l1_duplicate_lines.len(),
            "missing_l1_pointers": audit.missing_l1_pointers,
            "cleanup_dry_run": {
                "new_l1_lines": cleanup_dry.new_l1_lines,
                "added_missing_pointers": cleanup_dry.added_missing_pointers,
                "backup_path": cleanup_dry.backup_path,
            },
            "l4_sessions": audit.l4_sessions,
            "recall_hits": recall_hits.iter().map(|hit| json!({
                "session": hit.session,
                "score": hit.score,
                "excerpt": truncate_chars(&hit.excerpt, 160),
            })).collect::<Vec<_>>(),
        })
    );
    Ok(())
}

async fn vision_smoke(image_arg: Option<String>) -> Result<()> {
    let root = workspace_root()?;
    let cfg = VisionConfig::from_env(&root)?;
    let temp;
    let image = if let Some(path) = image_arg {
        PathBuf::from(path)
    } else {
        temp = tempfile::tempdir()?;
        let path = temp.path().join("koda_vision_smoke.jpg");
        generate_vision_smoke_image(&path)?;
        path
    };
    let mut req = VisionRequest::new(
        &image,
        "This is a smoke test image. Reply with the visible uppercase words only.",
    );
    req.timeout_secs = Some(cfg.timeout_secs.max(60));
    let text = ask_vision(&cfg, &req).await?;
    println!(
        "{}",
        json!({
            "ok": true,
            "backend": cfg.backend,
            "model": cfg.model,
            "image": image.display().to_string(),
            "answer_preview": truncate_chars(&text, 300),
            "contains_koda": text.to_ascii_uppercase().contains("KODA"),
            "contains_vision": text.to_ascii_uppercase().contains("VISION"),
        })
    );
    Ok(())
}

fn generate_vision_smoke_image(path: &Path) -> Result<()> {
    let script = r#"import sys
from PIL import Image, ImageDraw, ImageFont
img = Image.new('RGB', (900, 320), (245, 247, 238))
draw = ImageDraw.Draw(img)
try:
    font = ImageFont.truetype('/System/Library/Fonts/Supplemental/Arial Bold.ttf', 76)
except Exception:
    font = ImageFont.load_default()
draw.rectangle((28, 28, 872, 292), outline=(18, 45, 38), width=6)
draw.text((86, 112), 'KODA VISION OK', fill=(18, 45, 38), font=font)
img.save(sys.argv[1], 'JPEG', quality=90)
"#;
    let output = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(path)
        .output()
        .context("generate vision smoke image with python3/Pillow")?;
    if !output.status.success() {
        bail!(
            "generate vision smoke image failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn workspace_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .context("resolve workspace root")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

async fn acp_client_smoke() -> Result<()> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Command;

    let root = tempfile::tempdir()?;
    let mock_openai = spawn_mock_openai_server(Duration::from_millis(2_000)).await?;
    fs::create_dir_all(root.path().join("assets"))?;
    fs::write(root.path().join("assets/tools_schema.json"), "[]")?;
    fs::write(
        root.path().join("assets/sys_prompt.txt"),
        "You are GenericAgent.",
    )?;
    fs::write(
        root.path().join(".env"),
        format!(
            "OPENAI_BASE_URL={mock_openai}\nOPENAI_API_KEY=sk-redacted\nOPENAI_MODEL=mock\nOPENAI_TIMEOUT_SECS=10\n"
        ),
    )?;

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .context("resolve workspace root")?
        .join("Cargo.toml");
    let mut child = Command::new("cargo")
        .args([
            "run",
            "-q",
            "--manifest-path",
            manifest.to_str().context("workspace manifest path utf8")?,
            "-p",
            "koda-agent-cli",
            "--",
            "serve-acp",
        ])
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn serve-acp child")?;
    let mut stdin = child.stdin.take().context("child stdin")?;
    let stdout = child.stdout.take().context("child stdout")?;
    let mut lines = BufReader::new(stdout).lines();

    async fn write_req<W: AsyncWriteExt + Unpin>(stdin: &mut W, req: Value) -> Result<()> {
        stdin.write_all(format!("{req}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn read_until_id<R: AsyncBufReadExt + Unpin>(
        lines: &mut tokio::io::Lines<R>,
        wanted_id: i64,
        timeout_secs: u64,
    ) -> Result<(Vec<Value>, Value)> {
        let mut notifications = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("timed out waiting for ACP response id {wanted_id}");
            }
            let Some(line) = tokio::time::timeout(remaining, lines.next_line()).await?? else {
                bail!("ACP child stdout closed while waiting for id {wanted_id}");
            };
            let value: Value = serde_json::from_str(&line)
                .with_context(|| format!("parse ACP JSONL line: {line}"))?;
            if value.get("method").and_then(Value::as_str) == Some("session/update") {
                notifications.push(value);
                continue;
            }
            if value.get("id").and_then(Value::as_i64) == Some(wanted_id) {
                return Ok((notifications, value));
            }
        }
    }

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await?;
    let (_, init) = read_until_id(&mut lines, 1, 30).await?;
    let seen_init = init
        .pointer("/result/agentInfo/name")
        .and_then(Value::as_str)
        == Some("genericagent-acp");

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":root.path().display().to_string()}}),
    )
    .await?;
    let (_, created) = read_until_id(&mut lines, 2, 30).await?;
    let session_id = created
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .context("ACP session/new missing sessionId")?
        .to_string();
    let seen_session = session_id.starts_with("ga_");

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"/llms"}]}}),
    )
    .await?;
    let (updates, prompt_result) = read_until_id(&mut lines, 3, 30).await?;
    let seen_prompt_result = prompt_result
        .pointer("/result/stopReason")
        .and_then(Value::as_str)
        == Some("end_turn");
    let seen_update = updates
        .iter()
        .any(|value| value.get("method").and_then(Value::as_str) == Some("session/update"));

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":4,"method":"session/list","params":{}}),
    )
    .await?;
    let (_, list_result) = read_until_id(&mut lines, 4, 30).await?;
    let seen_list = list_result.pointer("/error/code").and_then(Value::as_i64) == Some(-32601)
        && list_result
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|m| m.contains("session/list not supported"));

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":5,"method":"session/load","params":{"sessionId":"ga_missing"}}),
    )
    .await?;
    let (_, load_result) = read_until_id(&mut lines, 5, 30).await?;
    let seen_load = load_result.pointer("/error/code").and_then(Value::as_i64) == Some(-32601)
        && load_result
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|m| m.contains("session/load not supported"));

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":6,"method":"session/cancel","params":{"sessionId":session_id}}),
    )
    .await?;
    let (_, cancel_result) = read_until_id(&mut lines, 6, 30).await?;
    let seen_cancel = cancel_result.pointer("/result/ok").and_then(Value::as_bool) == Some(true);

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":20,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"slow hello"}]}}),
    )
    .await?;
    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":21,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"second while busy"}]}}),
    )
    .await?;
    let (_, concurrent_busy) = read_until_id(&mut lines, 21, 5).await?;
    let seen_concurrent_busy = concurrent_busy
        .pointer("/error/code")
        .and_then(Value::as_i64)
        == Some(-32603)
        && concurrent_busy
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|m| m.contains("active prompt"));

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":22,"method":"session/cancel","params":{"sessionId":session_id}}),
    )
    .await?;
    let (_, cancel_running) = read_until_id(&mut lines, 22, 5).await?;
    let seen_cancel_running = cancel_running
        .pointer("/result/ok")
        .and_then(Value::as_bool)
        == Some(true);
    let (running_updates, running_done) = read_until_id(&mut lines, 20, 30).await?;
    let seen_running_done = running_done
        .pointer("/result/stopReason")
        .and_then(Value::as_str)
        .is_some();
    let seen_running_update = running_updates
        .iter()
        .any(|value| value.get("method").and_then(Value::as_str) == Some("session/update"));

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"sessionId":"ga_missing","prompt":[{"type":"text","text":"hello"}]}}),
    )
    .await?;
    let (_, unknown_session) = read_until_id(&mut lines, 7, 30).await?;
    let seen_unknown_session = unknown_session
        .pointer("/error/code")
        .and_then(Value::as_i64)
        == Some(-32602)
        && unknown_session
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|m| m.contains("unknown sessionId"));

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":8,"method":"session/prompt","params":{"sessionId":session_id,"prompt":"hello"}}),
    )
    .await?;
    let (_, invalid_prompt) = read_until_id(&mut lines, 8, 30).await?;
    let seen_invalid_prompt = invalid_prompt
        .pointer("/error/code")
        .and_then(Value::as_i64)
        == Some(-32602)
        && invalid_prompt
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|m| m.contains("prompt must be an array"));

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":9,"method":"unknown/method","params":{}}),
    )
    .await?;
    let (_, unknown_method) = read_until_id(&mut lines, 9, 30).await?;
    let seen_unknown_method = unknown_method
        .pointer("/error/code")
        .and_then(Value::as_i64)
        == Some(-32601)
        && unknown_method
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|m| m.contains("method not found"));

    write_req(&mut stdin, json!({"jsonrpc":"2.0","id":10,"params":{}})).await?;
    let (_, invalid_request) = read_until_id(&mut lines, 10, 30).await?;
    let seen_invalid_request = invalid_request
        .pointer("/error/code")
        .and_then(Value::as_i64)
        == Some(-32600)
        && invalid_request
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|m| m.contains("invalid request"));

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":11,"method":"session/close","params":{"sessionId":session_id}}),
    )
    .await?;
    let (_, close_result) = read_until_id(&mut lines, 11, 30).await?;
    let seen_close = close_result.get("result") == Some(&json!({}));

    write_req(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":12,"method":"shutdown","params":{}}),
    )
    .await?;
    stdin.shutdown().await?;

    let status = child.wait().await?;
    let seen_shutdown = status.success();
    if !(seen_init
        && seen_session
        && seen_prompt_result
        && seen_update
        && seen_list
        && seen_load
        && seen_cancel
        && seen_concurrent_busy
        && seen_cancel_running
        && seen_running_done
        && seen_running_update
        && seen_unknown_session
        && seen_invalid_prompt
        && seen_unknown_method
        && seen_invalid_request
        && seen_close
        && seen_shutdown)
    {
        bail!(
            "ACP client smoke failed: init={seen_init} session={seen_session} prompt={seen_prompt_result} update={seen_update} list={seen_list} load={seen_load} cancel={seen_cancel} concurrent_busy={seen_concurrent_busy} cancel_running={seen_cancel_running} running_done={seen_running_done} running_update={seen_running_update} unknown_session={seen_unknown_session} invalid_prompt={seen_invalid_prompt} unknown_method={seen_unknown_method} invalid_request={seen_invalid_request} close={seen_close} shutdown={seen_shutdown}"
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "initialize": seen_init,
            "session_new": seen_session,
            "session_prompt": seen_prompt_result,
            "session_update": seen_update,
            "session_list": seen_list,
            "session_load": seen_load,
            "session_cancel": seen_cancel,
            "concurrent_busy": seen_concurrent_busy,
            "cancel_running": seen_cancel_running,
            "running_prompt_done": seen_running_done,
            "running_prompt_update": seen_running_update,
            "unknown_session": seen_unknown_session,
            "invalid_prompt": seen_invalid_prompt,
            "unknown_method": seen_unknown_method,
            "invalid_request": seen_invalid_request,
            "session_close": seen_close,
            "shutdown": seen_shutdown
        }))?
    );
    Ok(())
}

async fn spawn_mock_openai_server(delay: Duration) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(delay).await;
                let body = json!({
                    "id":"chatcmpl-koda-smoke",
                    "object":"chat.completion",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"mock acp response"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Ok(format!("http://{addr}"))
}

async fn browser_smoke() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let tabs: Vec<Value> = client
        .get("http://127.0.0.1:9222/json")
        .send()
        .await
        .context("connect to Chrome CDP at 127.0.0.1:9222; start Chrome with --remote-debugging-port=9222")?
        .json()
        .await
        .context("decode /json tabs")?;
    let pages = tabs
        .into_iter()
        .filter(|t| t.get("type").and_then(Value::as_str).unwrap_or("page") == "page")
        .collect::<Vec<_>>();
    if pages.is_empty() {
        bail!("CDP is reachable but no page tabs are available");
    }
    let active = pages
        .iter()
        .find(|t| {
            t.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| !u.starts_with("devtools://"))
        })
        .unwrap_or(&pages[0]);
    let ws = active
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .context("selected tab has no webSocketDebuggerUrl")?;
    let title = cdp_eval(ws, "document.title", false).await?;
    let href = cdp_eval(ws, "location.href", false).await?;
    let batch_second = cdp_eval(
        ws,
        "(() => { const R=[{ok:true,data:[{id:123,url:location.href}]}]; return R[0].data[0].url; })()",
        false,
    )
    .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "tabs_count": pages.len(),
            "active_tab": {
                "id": active.get("id").cloned().unwrap_or(Value::Null),
                "url": active.get("url").cloned().unwrap_or(Value::Null),
                "title": active.get("title").cloned().unwrap_or(Value::Null),
            },
            "runtime_evaluate": {
                "document_title": title,
                "location_href": href,
                "batch_ref_equivalent": batch_second,
            }
        }))?
    );
    Ok(())
}

async fn tmwd_real_matrix_smoke() -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(700))
        .timeout(Duration::from_secs(25))
        .build()?;
    wait_for_tmwd_master(&client).await?;
    let sessions = wait_for_tmwd_sessions(&client).await?;
    let tab_id = sessions
        .iter()
        .find(|s| {
            s.get("url")
                .and_then(Value::as_str)
                .is_some_and(|url| url.starts_with("http://") || url.starts_with("https://"))
        })
        .or_else(|| sessions.first())
        .and_then(|s| s.get("id").and_then(value_as_id))
        .context(
            "TMWebDriver master is up but no scriptable extension tab sessions are connected",
        )?;

    let server = MatrixHttpServer::start().await?;
    let matrix_url = server.url("/matrix");
    let cookie_url = server.url("/");
    let nonce = server.nonce.clone();

    let tabs = tmwd_link(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":{"cmd":"tabs"},"timeout":10}),
    )
    .await
    .context("matrix tabs command failed")?;
    let management = tmwd_link(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":{"cmd":"management","method":"list"},"timeout":10}),
    )
    .await
    .context("matrix management.list command failed")?;

    let navigation = tmwd_link_raw(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":format!("location.href = {}; 'navigating';", serde_json::to_string(&matrix_url)?),"timeout":10}),
    )
    .await
    .context("matrix navigation command failed")?;
    let ready = wait_for_tmwd_page_ready(&client, &tab_id, &nonce).await?;

    let dom_action = tmwd_link(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":r#"
const input = document.querySelector('#name');
input.value = 'Koda Matrix User';
input.dispatchEvent(new Event('input', { bubbles: true }));
document.querySelector('#save').click();
({ value: input.value, result: document.querySelector('#result').textContent, itemCount: document.querySelectorAll('[data-row]').length })
"#,"timeout":10}),
    )
    .await
    .context("matrix DOM action failed")?;
    ensure_json_string_contains(
        &dom_action,
        "/result",
        "Saved: Koda Matrix User",
        "dom_action result",
    )?;

    let iframe = tmwd_link(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":"frames[0].document.body.dataset.kodaFrame + ':' + frames[0].document.querySelector('#frame-value').textContent","timeout":10}),
    )
    .await
    .context("matrix same-origin iframe read failed")?;
    if !iframe
        .as_str()
        .is_some_and(|s| s.contains("ready:Frame OK"))
    {
        bail!("matrix iframe expected ready marker, got {iframe}");
    }

    let cross_origin_iframe = tmwd_link(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":r#"
(() => {
  const f = document.querySelector('#cross-child');
  const out = { src: f.src, blocked: false, error: null, visible: !!(f.offsetWidth || f.offsetHeight || f.getClientRects().length) };
  try { out.text = f.contentWindow.document.body.innerText; }
  catch (e) { out.blocked = true; out.error = e.message || String(e); }
  return out;
})()
"#,"timeout":10}),
    )
    .await
    .context("matrix cross-origin iframe guard failed")?;
    if !cross_origin_iframe
        .get("blocked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!(
            "matrix cross-origin iframe should be blocked by browser policy: {cross_origin_iframe}"
        );
    }

    let autofill_candidates = tmwd_link(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":r#"
[...document.querySelectorAll('input')].filter(e => e.autocomplete || e.type === 'password').map(e => ({
  id: e.id,
  type: e.type,
  autocomplete: e.autocomplete,
  protected: e.readOnly || e.matches(':disabled'),
  visible: !!(e.offsetWidth || e.offsetHeight || e.getClientRects().length)
}))
"#,"timeout":10}),
    )
    .await
    .context("matrix autofill candidate detection failed")?;
    let autofill_seen = autofill_candidates.as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item.get("id").and_then(Value::as_str) == Some("password"))
            && items
                .iter()
                .any(|item| item.get("id").and_then(Value::as_str) == Some("name"))
    });
    if !autofill_seen {
        bail!("matrix autofill candidates missing expected fields: {autofill_candidates}");
    }

    let before_download_hits = server.hit_count("/download");
    let download_click = tmwd_link_raw(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":"document.querySelector('#download').click(); 'download-clicked';","timeout":10}),
    )
    .await
    .context("matrix download click failed")?;
    let download_hits = wait_for_matrix_hit(&server, "/download", before_download_hits).await?;

    let popup = tmwd_link_raw(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":"document.querySelector('#popup').click(); 'popup-clicked';","timeout":10}),
    )
    .await
    .context("matrix popup/new-tab command failed")?;
    let popup_seen = popup
        .get("newTabs")
        .and_then(Value::as_array)
        .is_some_and(|tabs| !tabs.is_empty());
    let popup_status = if popup_seen {
        "reported"
    } else {
        // Some browsers block script-origin target=_blank without a trusted user gesture.
        // Keep the matrix deterministic while still surfacing the bridge's newTabs payload.
        "blocked_or_not_reported"
    };

    let cdp = tmwd_link(
        &client,
        json!({
            "cmd":"execute_js",
            "sessionId":tab_id,
            "code":{"cmd":"cdp","method":"Runtime.evaluate","params":{"expression":"document.querySelector('[data-koda-matrix]').dataset.kodaMatrix","returnByValue":true}},
            "timeout":10
        }),
    )
    .await
    .context("matrix CDP Runtime.evaluate failed")?;
    ensure_json_string_contains(&cdp, "/result/value", "true", "cdp matrix marker")?;

    let screenshot = tmwd_link(
        &client,
        json!({
            "cmd":"execute_js",
            "sessionId":tab_id,
            "code":{"cmd":"cdp","method":"Page.captureScreenshot","params":{"format":"png"}},
            "timeout":10
        }),
    )
    .await
    .context("matrix CDP Page.captureScreenshot failed")?;
    let screenshot_len = screenshot
        .get("data")
        .and_then(Value::as_str)
        .map(str::len)
        .unwrap_or_default();
    if screenshot_len < 100 {
        bail!("matrix screenshot returned too little data: {screenshot}");
    }

    let batch = tmwd_link(
        &client,
        json!({
            "cmd":"execute_js",
            "sessionId":tab_id,
            "code":{"cmd":"batch","commands":[
                {"cmd":"cdp","method":"Runtime.evaluate","params":{"expression":"'document.title'","returnByValue":true}},
                {"cmd":"cdp","method":"Runtime.evaluate","params":{"expression":"$0.result.value","returnByValue":true}}
            ]},
            "timeout":10
        }),
    )
    .await
    .context("matrix batch command failed")?;
    ensure_json_string_contains(
        &batch,
        "/1/result/value",
        "Koda Browser Matrix",
        "batch title",
    )?;

    let cookies = tmwd_link(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":{"cmd":"cookies","url":cookie_url},"timeout":10}),
    )
    .await
    .context("matrix cookies command failed")?;
    let cookie_seen = cookies.as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item.get("name").and_then(Value::as_str) == Some("koda_matrix_token"))
    });
    if !cookie_seen {
        bail!("matrix cookie command did not return koda_matrix_token: {cookies}");
    }

    let self_disable_guard = if let Some(ext_id) = tmwd_self_extension_id_from_metadata(&management)
    {
        tmwd_link_allow_error(
            &client,
            json!({"cmd":"execute_js","sessionId":tab_id,"code":{"cmd":"management","method":"disable","extId":ext_id},"timeout":10}),
        )
        .await
        .context("matrix self-disable guard command failed")?
    } else {
        json!({"skipped":"installed extension did not expose isSelf metadata; reload assets/tmwd_cdp_bridge"})
    };

    let mut content_settings =
        json!({"skipped":"set KODA_TMWD_SMOKE_MUTATE=1 to include contentSettings allow/restore"});
    if env::var("KODA_TMWD_SMOKE_MUTATE").ok().as_deref() == Some("1") {
        let allow = tmwd_link(
            &client,
            json!({"cmd":"execute_js","sessionId":tab_id,"code":{"cmd":"contentSettings","type":"automaticDownloads","pattern":"<all_urls>","setting":"allow"},"timeout":10}),
        )
        .await
        .context("matrix contentSettings allow failed")?;
        let restore = tmwd_link(
            &client,
            json!({"cmd":"execute_js","sessionId":tab_id,"code":{"cmd":"contentSettings","type":"automaticDownloads","pattern":"<all_urls>","setting":"ask"},"timeout":10}),
        )
        .await
        .context("matrix contentSettings restore failed")?;
        content_settings = json!({"mutated":true,"allow":allow,"restore":restore});
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "matrix_url": matrix_url,
            "selected_session": tab_id,
            "cases": {
                "sessions": {"count": sessions.len()},
                "tabs": summarize_tabs(&tabs, 5),
                "navigation": summarize_tmwd_raw(&navigation),
                "ready": ready,
                "dom_action": dom_action,
                "same_origin_iframe": iframe,
                "cross_origin_iframe": cross_origin_iframe,
                "autofill_candidates": autofill_candidates,
                "download": {"hits": download_hits, "raw": summarize_tmwd_raw(&download_click)},
                "popup_new_tab": {"status": popup_status, "raw": summarize_tmwd_raw(&popup)},
                "cdp_runtime_evaluate": summarize_cdp_runtime(&cdp),
                "cdp_capture_screenshot": summarize_cdp_screenshot(&screenshot),
                "batch_refs": summarize_tmwd_value(&batch, 2),
                "cookies": summarize_cookies(&cookies, 5),
                "management": summarize_tmwd_value(&management, 5),
                "self_disable_guard": summarize_tmwd_raw(&self_disable_guard),
                "contentSettings": content_settings,
            }
        }))?
    );
    Ok(())
}

async fn wait_for_tmwd_page_ready(
    client: &reqwest::Client,
    tab_id: &str,
    nonce: &str,
) -> Result<Value> {
    let nonce_json = serde_json::to_string(nonce)?;
    let script = format!(
        "({{href: location.href, ready: document.readyState, title: document.title, marker: document.body && document.body.dataset.kodaMatrix, nonceOk: location.href.includes({nonce_json})}})"
    );
    let mut last = Value::Null;
    for _ in 0..40 {
        match tmwd_link(
            client,
            json!({"cmd":"execute_js","sessionId":tab_id,"code":script,"timeout":5}),
        )
        .await
        {
            Ok(value) => {
                let ready = value.get("ready").and_then(Value::as_str) == Some("complete");
                let marker = value.get("marker").and_then(Value::as_str) == Some("true");
                let nonce_ok = value
                    .get("nonceOk")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if ready && marker && nonce_ok {
                    return Ok(value);
                }
                last = value;
            }
            Err(e) => last = json!({"error": format!("{e:#}")}),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("matrix page did not become ready; last={last}")
}

struct MatrixHttpServer {
    base_url: String,
    nonce: String,
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl MatrixHttpServer {
    async fn start() -> Result<Self> {
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
        );
        let hits = Arc::new(Mutex::new(HashMap::new()));
        let cross_origin_base_url = spawn_matrix_http_listener(
            nonce.clone(),
            String::new(),
            hits.clone(),
            MatrixSiteKind::CrossOrigin,
        )
        .await?;
        let base_url = spawn_matrix_http_listener(
            nonce.clone(),
            cross_origin_base_url.clone(),
            hits.clone(),
            MatrixSiteKind::Main,
        )
        .await?;
        Ok(Self {
            base_url,
            nonce,
            hits,
        })
    }

    fn url(&self, path: &str) -> String {
        if path == "/matrix" {
            format!("{}/matrix?nonce={}", self.base_url, self.nonce)
        } else {
            format!("{}{}", self.base_url, path)
        }
    }

    fn hit_count(&self, path: &str) -> usize {
        self.hits
            .lock()
            .expect("matrix hit lock")
            .get(path)
            .copied()
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy)]
enum MatrixSiteKind {
    Main,
    CrossOrigin,
}

async fn spawn_matrix_http_listener(
    nonce: String,
    cross_origin_base_url: String,
    hits: Arc<Mutex<HashMap<String, usize>>>,
    kind: MatrixSiteKind,
) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let nonce = nonce.clone();
            let cross_origin_base_url = cross_origin_base_url.clone();
            let hits = hits.clone();
            tokio::spawn(async move {
                let mut buf = [0_u8; 2048];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let clean_path = path.split('?').next().unwrap_or(path).to_string();
                *hits
                    .lock()
                    .expect("matrix hit lock")
                    .entry(clean_path)
                    .or_insert(0) += 1;
                let (status, content_type, extra_headers, body) = match kind {
                    MatrixSiteKind::Main => {
                        matrix_http_response(path, &nonce, &cross_origin_base_url)
                    }
                    MatrixSiteKind::CrossOrigin => matrix_cross_origin_response(path, &nonce),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n{extra_headers}\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    Ok(format!("http://{}", addr))
}

async fn wait_for_matrix_hit(
    server: &MatrixHttpServer,
    path: &str,
    previous: usize,
) -> Result<usize> {
    for _ in 0..40 {
        let hits = server.hit_count(path);
        if hits > previous {
            return Ok(hits);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("matrix endpoint {path} was not requested after browser action")
}

fn matrix_http_response(
    path: &str,
    nonce: &str,
    cross_origin_base_url: &str,
) -> (&'static str, &'static str, String, String) {
    let clean_path = path.split('?').next().unwrap_or(path);
    match clean_path {
        "/" | "/matrix" => (
            "200 OK",
            "text/html",
            "set-cookie: koda_matrix_token=bridge-ok; Path=/; SameSite=Lax\r\n".to_string(),
            format!(
                r#"<!doctype html>
<html data-koda-matrix="true"><head><meta charset="utf-8"><title>Koda Browser Matrix</title></head>
<body data-koda-matrix="true" data-nonce="{nonce}">
  <h1>Koda Browser Matrix</h1>
  <form id="profile"><label>Name <input id="name" name="name" autocomplete="name"></label><label>Password <input id="password" name="password" type="password" autocomplete="current-password"></label><button id="save" type="button">Save</button></form>
  <output id="result">Pending</output>
  <ul id="rows"><li data-row="1">Alpha</li><li data-row="2">Beta</li><li data-row="3">Gamma</li></ul>
  <iframe id="child" src="/frame?nonce={nonce}"></iframe>
  <iframe id="cross-child" src="{cross_origin_base_url}/xframe?nonce={nonce}"></iframe>
  <a id="popup" href="/popup?nonce={nonce}" target="_blank">Open popup</a>
  <a id="download" href="/download?nonce={nonce}" download="koda-matrix.txt">Download</a>
  <script>
    document.querySelector('#save').addEventListener('click', () => {{
      document.querySelector('#result').textContent = 'Saved: ' + document.querySelector('#name').value;
    }});
  </script>
</body></html>"#
            ),
        ),
        "/frame" => (
            "200 OK",
            "text/html",
            String::new(),
            format!(
                r#"<!doctype html><html><body data-koda-frame="ready" data-nonce="{nonce}"><strong id="frame-value">Frame OK</strong></body></html>"#
            ),
        ),
        "/popup" => (
            "200 OK",
            "text/html",
            String::new(),
            format!(
                r#"<!doctype html><title>Koda Popup</title><main data-popup-nonce="{nonce}">Popup OK</main>"#
            ),
        ),
        "/download" => (
            "200 OK",
            "text/plain",
            "content-disposition: attachment; filename=\"koda-matrix.txt\"\r\n".to_string(),
            format!("koda matrix download {nonce}\n"),
        ),
        _ => (
            "404 Not Found",
            "text/plain",
            String::new(),
            "not found".to_string(),
        ),
    }
}

fn matrix_cross_origin_response(
    path: &str,
    nonce: &str,
) -> (&'static str, &'static str, String, String) {
    let clean_path = path.split('?').next().unwrap_or(path);
    match clean_path {
        "/xframe" => (
            "200 OK",
            "text/html",
            "x-frame-options: ALLOWALL
"
            .to_string(),
            format!(
                r#"<!doctype html><html><body data-cross-origin="ready" data-nonce="{nonce}"><strong>Cross Origin Frame OK</strong></body></html>"#
            ),
        ),
        _ => (
            "404 Not Found",
            "text/plain",
            String::new(),
            "not found".to_string(),
        ),
    }
}

fn ensure_json_string_contains(
    value: &Value,
    pointer: &str,
    needle: &str,
    label: &str,
) -> Result<()> {
    let actual = value
        .pointer(pointer)
        .map(value_to_string)
        .unwrap_or_default();
    if !actual.contains(needle) {
        bail!("{label} expected {pointer} to contain {needle:?}, got {value}");
    }
    Ok(())
}

fn summarize_tmwd_raw(value: &Value) -> Value {
    json!({
        "data": value.get("data").cloned().unwrap_or_else(|| value.clone()),
        "new_tabs_count": value.get("newTabs").and_then(Value::as_array).map_or(0, Vec::len),
        "error": value.get("error").cloned().unwrap_or(Value::Null),
    })
}

async fn tmwd_extension_smoke() -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(700))
        .timeout(Duration::from_secs(20))
        .build()?;
    wait_for_tmwd_master(&client).await?;
    let sessions = wait_for_tmwd_sessions(&client).await?;
    let tab_id = sessions
        .iter()
        .find_map(|s| s.get("id").and_then(value_as_id))
        .context("TMWebDriver master is up but no extension tab sessions are connected")?;

    let tabs = tmwd_link(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":{"cmd":"tabs"},"timeout":10}),
    )
    .await
    .context("tmwd tabs command failed")?;
    let management = tmwd_link(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":{"cmd":"management","method":"list"},"timeout":10}),
    )
    .await
    .context("tmwd management.list command failed; check the Edge extension permissions")?;
    let management_safety = if let Some(ext_id) = tmwd_self_extension_id_from_metadata(&management)
    {
        let blocked = tmwd_link_allow_error(
            &client,
            json!({"cmd":"execute_js","sessionId":tab_id,"code":{"cmd":"management","method":"disable","extId":ext_id},"timeout":10}),
        )
        .await
        .context("tmwd management.disable self-protection command failed")?;
        json!({"self_disable_without_confirm": summarize_tmwd_value(&blocked, 3)})
    } else {
        json!({"skipped":"installed tmwd bridge did not expose isSelf metadata; reload the unpacked extension from assets/tmwd_cdp_bridge to enable self-disable guard validation"})
    };
    let cdp = tmwd_link(
        &client,
        json!({
            "cmd":"execute_js",
            "sessionId":tab_id,
            "code":{"cmd":"cdp","method":"Runtime.evaluate","params":{"expression":"location.href","returnByValue":true}},
            "timeout":10
        }),
    )
    .await
    .context("tmwd cdp Runtime.evaluate command failed")?;
    let cookies = tmwd_link(
        &client,
        json!({"cmd":"execute_js","sessionId":tab_id,"code":{"cmd":"cookies"},"timeout":10}),
    )
    .await
    .context("tmwd cookies command failed")?;

    let mut content_settings =
        json!({"skipped": "set KODA_TMWD_SMOKE_MUTATE=1 to allow contentSettings mutation"});
    if env::var("KODA_TMWD_SMOKE_MUTATE").ok().as_deref() == Some("1") {
        let allow = tmwd_link(
            &client,
            json!({
                "cmd":"execute_js",
                "sessionId":tab_id,
                "code":{"cmd":"contentSettings","type":"automaticDownloads","pattern":"<all_urls>","setting":"allow"},
                "timeout":10
            }),
        )
        .await
        .context("tmwd contentSettings allow command failed")?;
        let ask = tmwd_link(
            &client,
            json!({
                "cmd":"execute_js",
                "sessionId":tab_id,
                "code":{"cmd":"contentSettings","type":"automaticDownloads","pattern":"<all_urls>","setting":"ask"},
                "timeout":10
            }),
        )
        .await
        .context("tmwd contentSettings restore-to-ask command failed")?;
        content_settings = json!({"mutated": true, "allow": allow, "restore": ask});
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "master": "http://127.0.0.1:18766/link",
            "selected_session": tab_id,
            "sessions_count": sessions.len(),
            "tabs": summarize_tabs(&tabs, 3),
            "management": summarize_tmwd_value(&management, 5),
            "management_safety": management_safety,
            "cdp_runtime_evaluate": summarize_cdp_runtime(&cdp),
            "cookies": summarize_cookies(&cookies, 3),
            "contentSettings": content_settings,
        }))?
    );
    Ok(())
}

async fn rich_monitor_smoke() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let test_url = "data:text/html;charset=utf-8,%3C!doctype%20html%3E%3Ctitle%3EKoda%20Rich%20Monitor%20Smoke%3C/title%3E%3Cmain%20id%3Dapp%3E%3Ch1%3EKoda%20Rich%20Monitor%3C/h1%3E%3Cp%3Ebaseline%20content%3C/p%3E%3C/main%3E";
    let created: Value = client
        .put(format!("http://127.0.0.1:9222/json/new?{test_url}"))
        .send()
        .await
        .context("connect to CDP /json/new; start Edge/Chrome with --remote-debugging-port=9222")?
        .error_for_status()?
        .json()
        .await
        .context("decode created CDP target")?;
    let tab_id = created
        .get("id")
        .and_then(Value::as_str)
        .context("created target has no id")?
        .to_string();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let root = unique_temp_root("koda-rich-monitor-smoke")?;
    let cfg = smoke_agent_config(&root);
    let dispatcher = GenericToolDispatcher::new(cfg);
    let response = AgentResponse {
        thinking: String::new(),
        content: String::new(),
        tool_calls: vec![],
        raw: Value::Null,
    };

    let no_change = run_web_execute(
        &dispatcher,
        &response,
        json!({"switch_tab_id":tab_id,"script":"document.querySelector('h1').textContent"}),
    )
    .await
    .context("no-change monitor case failed")?;
    ensure_status(&no_change, "success", "no_change")?;
    ensure_contains(&no_change, "/diff", "页面无变化", "no_change diff")?;
    ensure_contains(
        &no_change,
        "/suggestion",
        "页面无明显变化",
        "no_change suggestion",
    )?;

    let dom_changed = run_web_execute(
        &dispatcher,
        &response,
        json!({"switch_tab_id":tab_id,"script":"const d=document.createElement('section'); d.id='added'; d.textContent='Added rich monitor content'; document.querySelector('#app').appendChild(d); d.outerHTML"}),
    )
    .await
    .context("DOM changed monitor case failed")?;
    ensure_status(&dom_changed, "success", "dom_changed")?;
    ensure_contains(&dom_changed, "/diff", "DOM变化量", "dom_changed diff")?;
    ensure_contains(
        &dom_changed,
        "/diff",
        "Added rich monitor content",
        "dom_changed top change",
    )?;

    let async_spa = run_web_execute(
        &dispatcher,
        &response,
        json!({"switch_tab_id":tab_id,"script":"setTimeout(()=>{const r=document.createElement('article'); r.id='route'; r.innerHTML='<h2>Async route loaded</h2><p>first chunk</p>'; document.querySelector('#app').appendChild(r);},150); setTimeout(()=>{document.querySelector('#route p').textContent='second async chunk'; document.querySelector('#route').setAttribute('data-route','done');},450); 'spa scheduled';"}),
    )
    .await
    .context("async SPA monitor case failed")?;
    ensure_status(&async_spa, "success", "async_spa")?;
    ensure_contains(&async_spa, "/diff", "DOM变化量", "async_spa diff")?;
    ensure_contains(
        &async_spa,
        "/page_changed_text",
        "Async route loaded",
        "async_spa changed text heading",
    )?;
    ensure_contains(
        &async_spa,
        "/page_changed_text",
        "second async chunk",
        "async_spa changed text",
    )?;

    let transient = run_web_execute(
        &dispatcher,
        &response,
        json!({"switch_tab_id":tab_id,"script":"const n=document.createElement('div'); n.textContent='Transient toast visible'; document.body.appendChild(n); setTimeout(()=>n.remove(), 650); 'transient scheduled';"}),
    )
    .await
    .context("transient monitor case failed")?;
    ensure_status(&transient, "success", "transient")?;
    let transient_seen = transient
        .get("transients")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.as_str().is_some_and(|s| s.contains("Transient toast")))
        });
    if !transient_seen {
        bail!("transient monitor did not capture expected text: {transient}");
    }

    let js_error = run_web_execute(
        &dispatcher,
        &response,
        json!({"switch_tab_id":tab_id,"script":"throw new Error('rich monitor boom')"}),
    )
    .await
    .context("JS error monitor case failed")?;
    ensure_status(&js_error, "failed", "js_error")?;
    ensure_contains(&js_error, "/error", "rich monitor boom", "js_error error")?;

    let new_tab = run_web_execute(
        &dispatcher,
        &response,
        json!({"switch_tab_id":tab_id,"script":"window.open('about:blank','_blank'); 'opened';"}),
    )
    .await
    .context("new-tab monitor case failed")?;
    ensure_status(&new_tab, "success", "new_tab")?;
    let new_tab_seen = new_tab
        .get("newTabs")
        .and_then(Value::as_array)
        .is_some_and(|tabs| !tabs.is_empty());
    if !new_tab_seen {
        bail!("new-tab monitor did not report newTabs: {new_tab}");
    }

    let reloaded = run_web_execute(
        &dispatcher,
        &response,
        json!({"switch_tab_id":tab_id,"script":"history.pushState({}, '', '#koda-reloaded'); 'navigating';"}),
    )
    .await
    .context("reload/navigation monitor case failed")?;
    ensure_status(&reloaded, "success", "reloaded")?;
    if !reloaded
        .get("reloaded")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("reload/navigation monitor did not set reloaded=true: {reloaded}");
    }
    ensure_contains(
        &reloaded,
        "/suggestion",
        "页面已刷新",
        "reloaded suggestion",
    )?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "target": {"id": tab_id, "url": "data:text/html,<local smoke page>"},
            "cases": {
                "no_change": summarize_monitor_case(&no_change),
                "dom_changed": summarize_monitor_case(&dom_changed),
                "async_spa": summarize_monitor_case(&async_spa),
                "transient": summarize_monitor_case(&transient),
                "js_error": summarize_monitor_case(&js_error),
                "new_tab": summarize_monitor_case(&new_tab),
                "reloaded": summarize_monitor_case(&reloaded),
            }
        }))?
    );
    let _ = fs::remove_dir_all(&root);
    Ok(())
}

async fn run_web_execute(
    dispatcher: &GenericToolDispatcher,
    response: &AgentResponse,
    args: Value,
) -> Result<Value> {
    Ok(dispatcher
        .dispatch("web_execute_js", args, response, 0)
        .await?
        .data)
}

fn ensure_status(value: &Value, expected: &str, label: &str) -> Result<()> {
    let actual = value.get("status").and_then(Value::as_str);
    if actual != Some(expected) {
        bail!("{label} expected status {expected}, got {value}");
    }
    Ok(())
}

fn ensure_contains(value: &Value, pointer: &str, needle: &str, label: &str) -> Result<()> {
    let actual = value.pointer(pointer).and_then(Value::as_str);
    if !actual.is_some_and(|s| s.contains(needle)) {
        bail!("{label} expected {pointer} to contain {needle:?}, got {value}");
    }
    Ok(())
}

fn summarize_monitor_case(value: &Value) -> Value {
    json!({
        "status": value.get("status").cloned().unwrap_or(Value::Null),
        "diff": value.get("diff").and_then(Value::as_str).map(|s| truncate_for_report(s, 180)).unwrap_or_default(),
        "page_changed_text": value.get("page_changed_text").and_then(Value::as_str).map(|s| truncate_for_report(s, 180)).unwrap_or_default(),
        "suggestion": value.get("suggestion").cloned().unwrap_or(Value::Null),
        "transients_count": value.get("transients").and_then(Value::as_array).map_or(0, Vec::len),
        "new_tabs_count": value.get("newTabs").and_then(Value::as_array).map_or(0, Vec::len),
        "error": value.get("error").and_then(Value::as_str).map(|s| truncate_for_report(s, 180)).unwrap_or_default(),
    })
}

fn truncate_for_report(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    format!("{}...", s.chars().take(max_chars).collect::<String>())
}

fn unique_temp_root(prefix: &str) -> Result<PathBuf> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let root = env::temp_dir().join(format!("{prefix}-{millis}-{}", std::process::id()));
    fs::create_dir_all(root.join("temp"))?;
    fs::create_dir_all(root.join("memory"))?;
    fs::create_dir_all(root.join("logs"))?;
    Ok(root)
}

fn smoke_agent_config(root: &Path) -> AgentConfig {
    AgentConfig {
        root_dir: root.into(),
        temp_dir: root.join("temp"),
        memory_dir: root.join("memory"),
        logs_dir: root.join("logs"),
        openai_base_url: "http://127.0.0.1/unused".into(),
        openai_api_key: "sk-redacted".into(),
        openai_model: "unused".into(),
        llm_api_style: "chat".into(),
        max_turns: 3,
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

async fn wait_for_tmwd_master(client: &reqwest::Client) -> Result<()> {
    for _ in 0..20 {
        if client
            .get("http://127.0.0.1:18766/")
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("TMWebDriver master is not reachable; run `make tmwebdriver` first")
}

async fn wait_for_tmwd_sessions(client: &reqwest::Client) -> Result<Vec<Value>> {
    for _ in 0..40 {
        let sessions = tmwd_link(client, json!({"cmd":"get_all_sessions"})).await?;
        if let Some(arr) = sessions.as_array()
            && !arr.is_empty()
        {
            return Ok(arr.clone());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("TMWebDriver master is reachable but the browser extension did not connect")
}

async fn tmwd_link(client: &reqwest::Client, body: Value) -> Result<Value> {
    let value: Value = client
        .post("http://127.0.0.1:18766/link")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let result = value.get("r").cloned().unwrap_or(Value::Null);
    if result.get("error").is_some() {
        bail!("{}", tmwd_error_message(&result));
    }
    Ok(result.get("data").cloned().unwrap_or(result))
}

async fn tmwd_link_raw(client: &reqwest::Client, body: Value) -> Result<Value> {
    let value: Value = client
        .post("http://127.0.0.1:18766/link")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let result = value.get("r").cloned().unwrap_or(Value::Null);
    if result.get("error").is_some() {
        bail!("{}", tmwd_error_message(&result));
    }
    Ok(result)
}

async fn tmwd_link_allow_error(client: &reqwest::Client, body: Value) -> Result<Value> {
    let value: Value = client
        .post("http://127.0.0.1:18766/link")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(value.get("r").cloned().unwrap_or(Value::Null))
}

fn tmwd_error_message(result: &Value) -> String {
    let Some(error) = result.get("error") else {
        return "tmwebdriver master error".to_string();
    };
    if let Some(s) = error.as_str() {
        return s.to_string();
    }
    error
        .get("message")
        .or_else(|| error.get("msg"))
        .map(value_to_string)
        .unwrap_or_else(|| value_to_string(error))
}

fn summarize_tmwd_value(value: &Value, max_items: usize) -> Value {
    let Some(arr) = value.as_array() else {
        return value.clone();
    };
    json!({
        "count": arr.len(),
        "sample": arr.iter().take(max_items).cloned().collect::<Vec<_>>(),
    })
}

#[cfg(test)]
fn tmwd_self_extension_id(management: &Value) -> Option<String> {
    let entries = management.as_array()?;
    entries.iter().find_map(|entry| {
        let is_self = entry
            .get("isSelf")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let likely_tmwd = entry
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.to_ascii_lowercase().contains("tmwd"));
        if is_self || likely_tmwd {
            entry.get("id").and_then(Value::as_str).map(str::to_string)
        } else {
            None
        }
    })
}

fn tmwd_self_extension_id_from_metadata(management: &Value) -> Option<String> {
    let entries = management.as_array()?;
    entries.iter().find_map(|entry| {
        entry
            .get("isSelf")
            .and_then(Value::as_bool)
            .filter(|is_self| *is_self)
            .and_then(|_| entry.get("id").and_then(Value::as_str).map(str::to_string))
    })
}

fn summarize_tabs(value: &Value, max_items: usize) -> Value {
    let Some(arr) = value.as_array() else {
        return value.clone();
    };
    json!({
        "count": arr.len(),
        "sample": arr.iter().take(max_items).map(|tab| json!({
            "id": tab.get("id").cloned().unwrap_or(Value::Null),
            "title": tab.get("title").cloned().unwrap_or(Value::Null),
            "url_origin": tab.get("url").and_then(Value::as_str).map(url_origin).unwrap_or_default(),
            "active": tab.get("active").cloned().unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
    })
}

fn summarize_cookies(value: &Value, max_items: usize) -> Value {
    let Some(arr) = value.as_array() else {
        return value.clone();
    };
    json!({
        "count": arr.len(),
        "sample": arr.iter().take(max_items).map(|cookie| json!({
            "name": cookie.get("name").cloned().unwrap_or(Value::Null),
            "domain": cookie.get("domain").cloned().unwrap_or(Value::Null),
            "path": cookie.get("path").cloned().unwrap_or(Value::Null),
            "secure": cookie.get("secure").cloned().unwrap_or(Value::Null),
            "httpOnly": cookie.get("httpOnly").cloned().unwrap_or(Value::Null),
            "value": "<redacted>",
        })).collect::<Vec<_>>(),
    })
}

fn summarize_cdp_runtime(value: &Value) -> Value {
    let mut out = value.clone();
    if let Some(v) = out.pointer_mut("/result/value")
        && let Some(s) = v.as_str()
        && (s.starts_with("http://") || s.starts_with("https://"))
    {
        *v = Value::String(url_origin(s));
    }
    out
}

fn summarize_cdp_screenshot(value: &Value) -> Value {
    json!({
        "bytes_base64": value.get("data").and_then(Value::as_str).map(str::len).unwrap_or_default(),
        "fallback": value.get("fallback").cloned().unwrap_or(Value::Null),
        "fallbackCause": value.get("fallbackCause").and_then(Value::as_str).map(|s| truncate_for_report(s, 120)).unwrap_or_default(),
    })
}

fn url_origin(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<non-url>".to_string();
    };
    let host = rest.split('/').next().unwrap_or_default();
    format!("{scheme}://{host}")
}

fn value_as_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|v| v.to_string()))
        .or_else(|| value.as_u64().map(|v| v.to_string()))
}

fn value_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

async fn cdp_eval(ws_url: &str, expression: &str, await_promise: bool) -> Result<Value> {
    let (mut ws, _) = connect_async(ws_url).await?;
    ws.send(Message::Text(
        json!({
            "id": 1,
            "method": "Runtime.evaluate",
            "params": {
                "expression": expression,
                "awaitPromise": await_promise,
                "returnByValue": true,
                "userGesture": true,
            }
        })
        .to_string()
        .into(),
    ))
    .await?;
    while let Some(frame) = ws.next().await {
        let frame = frame?;
        let text = match frame {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            _ => continue,
        };
        let value: Value = serde_json::from_str(&text)?;
        if value.get("id").and_then(Value::as_i64) != Some(1) {
            continue;
        }
        if let Some(e) = value.get("error") {
            bail!("CDP Runtime.evaluate error: {e}");
        }
        if let Some(details) = value.pointer("/result/exceptionDetails") {
            bail!("JavaScript exception: {details}");
        }
        return Ok(value
            .pointer("/result/result/value")
            .cloned()
            .or_else(|| value.pointer("/result/result/description").cloned())
            .unwrap_or(Value::Null));
    }
    bail!("CDP websocket closed before evaluation result")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmwd_bridge_management_js_has_self_disable_guard() {
        let js = fs::read_to_string("../../assets/tmwd_cdp_bridge/background.js").unwrap();
        assert!(js.contains("management.disable requires extId"));
        assert!(js.contains("confirmSelf !== true"));
        assert!(js.contains("Refusing to disable tmwd bridge extension"));
        assert!(js.contains("reconnectExpected: true"));
        assert!(js.contains("mayDisable: e.mayDisable"));
        assert!(js.contains("isSelf: e.id === chrome.runtime.id"));
    }

    #[test]
    fn tmwd_bridge_command_matrix_matches_upstream_surface() {
        let js = fs::read_to_string("../../assets/tmwd_cdp_bridge/background.js").unwrap();
        for needle in [
            "msg.cmd === 'cookies'",
            "msg.cmd === 'cdp'",
            "msg.cmd === 'batch'",
            "msg.cmd === 'tabs'",
            "msg.cmd === 'management'",
            "msg.cmd === 'contentSettings'",
            "handleCookies",
            "handleBatch",
            "handleCDP",
            "chrome.debugger.attach",
            "chrome.debugger.sendCommand",
            "handleCDPFallback",
            "tabs.captureVisibleTab",
            "scripting.executeScript",
            "cdpRemoteObject",
            "chrome.cookies.getAll",
            "chrome.contentSettings[type].set",
        ] {
            assert!(
                js.contains(needle),
                "missing bridge command surface: {needle}"
            );
        }
    }

    #[test]
    fn tmwd_content_bridge_routes_extension_only_commands() {
        let js = fs::read_to_string("../../assets/tmwd_cdp_bridge/content.js").unwrap();
        for needle in [
            "cmd === 'management'",
            "cmd === 'contentSettings'",
            "chrome.runtime.sendMessage({ cmd: 'management'",
            "chrome.runtime.sendMessage({ cmd: 'contentSettings'",
            "confirmSelf: req.confirmSelf",
            "pattern: req.pattern",
        ] {
            assert!(
                js.contains(needle),
                "missing content bridge route: {needle}"
            );
        }
    }

    #[test]
    fn tmwd_self_extension_id_prefers_self_then_tmwd_name() {
        let value = json!([
            {"id":"a","name":"Other","isSelf":false},
            {"id":"b","name":"Bridge","isSelf":true}
        ]);
        assert_eq!(tmwd_self_extension_id(&value).as_deref(), Some("b"));
        assert_eq!(
            tmwd_self_extension_id_from_metadata(&value).as_deref(),
            Some("b")
        );
        let value = json!([{"id":"c","name":"TMWD CDP Bridge","isSelf":false}]);
        assert_eq!(tmwd_self_extension_id(&value).as_deref(), Some("c"));
        assert_eq!(tmwd_self_extension_id_from_metadata(&value), None);
    }
}
