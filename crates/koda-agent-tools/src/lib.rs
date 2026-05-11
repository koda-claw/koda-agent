use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use koda_agent_core::{
    AgentConfig, AgentResponse, StepOutcome, ToolDispatcher,
    python_runtime::{PythonPurpose, python_unavailable_message, resolve_python},
    smart_format,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;
use tokio::{
    io::AsyncReadExt,
    process::Command,
    time::{sleep, timeout},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use walkdir::WalkDir;

#[derive(Clone)]
pub struct GenericToolDispatcher {
    cfg: AgentConfig,
    cwd: PathBuf,
    read_dirs: Arc<Mutex<Vec<PathBuf>>>,
}

impl GenericToolDispatcher {
    pub fn new(cfg: AgentConfig) -> Self {
        let cwd = cfg.workspace_dir.clone();
        Self {
            cfg,
            cwd,
            read_dirs: Arc::default(),
        }
    }
    fn abs(&self, p: impl AsRef<str>) -> PathBuf {
        let p = p.as_ref();
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }
    fn write_abs(&self, p: Option<&str>) -> PathBuf {
        let p = p.unwrap_or("").trim();
        if p.is_empty() || p == "/" || p == "." || p == "./" {
            return self.cfg.workspace_dir.join("index.html");
        }
        if matches!(p, "_stop" | "_stop_signal") {
            return self.cfg.temp_dir.join(p);
        }
        if p.starts_with('/') && !p[1..].contains('/') {
            return self.cfg.workspace_dir.join(&p[1..]);
        }
        let path = Path::new(p);
        if path.is_absolute() {
            if path.parent() == Some(Path::new("/")) {
                return self
                    .cfg
                    .workspace_dir
                    .join(path.file_name().unwrap_or_default());
            }
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }
}

#[async_trait]
impl ToolDispatcher for GenericToolDispatcher {
    async fn dispatch(
        &self,
        name: &str,
        args: Value,
        response: &AgentResponse,
        index: usize,
    ) -> Result<StepOutcome> {
        match name {
            "code_run" => self.do_code_run(args, response, index).await,
            "file_read" => self.do_file_read(args),
            "file_patch" => self.do_file_patch(args),
            "file_write" => self.do_file_write(args, response),
            "web_scan" => self.do_web_scan(args).await,
            "web_execute_js" => self.do_web_execute_js(args, response).await,
            "ask_user" => Ok(self.do_ask_user(args)),
            "update_working_checkpoint" => self.do_update_working_checkpoint(args),
            "start_long_term_update" => self.do_start_long_term_update(args),
            "bad_json" => Ok(StepOutcome::next(
                Value::Null,
                args.get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("bad_json"),
            )),
            other => Ok(StepOutcome::next(
                json!({"status":"error","msg":format!("未知工具: {other}")}),
                format!("未知工具 {other}"),
            )),
        }
    }
}

impl GenericToolDispatcher {
    async fn do_code_run(
        &self,
        args: Value,
        response: &AgentResponse,
        index: usize,
    ) -> Result<StepOutcome> {
        let typ = args
            .get("type")
            .or_else(|| args.get("code_type"))
            .and_then(Value::as_str)
            .unwrap_or("python");
        let code = args
            .get("code")
            .or_else(|| args.get("script"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| extract_code_block(&response.content, typ));
        let Some(mut code) = code else {
            return Ok(StepOutcome::next(
                json!({"status":"error","msg":"Code missing. Must use reply code block or 'script' arg."}),
                "\n",
            ));
        };
        let secs = args.get("timeout").and_then(Value::as_u64).unwrap_or(60);
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(|s| self.abs(s))
            .unwrap_or_else(|| self.cwd.clone());
        fs::create_dir_all(&cwd)?;
        let inline_eval = args
            .get("inline_eval")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if inline_eval && matches!(typ, "python" | "py") {
            if let Some(outcome) = self.handle_inline_eval_control(&code, index)? {
                return Ok(outcome);
            }
            code = inline_eval_wrapper(&code);
        } else if matches!(typ, "python" | "py") {
            code = self.with_code_run_header(&code);
        }
        let mut tmp_path = None;
        let mut cmd = if matches!(typ, "python" | "py") {
            let Some(py) = resolve_python(&self.cfg.home_dir, PythonPurpose::UserCode) else {
                return Ok(StepOutcome::next(
                    json!({"status":"error","code":"python_unavailable","msg":python_unavailable_message(),"fix":"koda-agent doctor; set KODA_PYTHON=/path/to/python; or use code_run type=bash"}),
                    "\n",
                ));
            };
            let mut tmp = NamedTempFile::with_suffix_in(".ai.py", &self.cwd)?;
            use std::io::Write;
            tmp.write_all(code.as_bytes())?;
            let (_file, path) = tmp.keep()?;
            tmp_path = Some(path.clone());
            let mut c = Command::new(&py.command.program);
            c.args(&py.command.args);
            c.arg("-X").arg("utf8").arg("-u").arg(path);
            c
        } else if matches!(typ, "bash" | "sh" | "shell") {
            let mut c = Command::new("bash");
            c.arg("-c").arg(code);
            c
        } else if matches!(typ, "powershell" | "ps1" | "pwsh") {
            let Some(pwsh) = find_program(&["pwsh", "powershell"]) else {
                return Ok(StepOutcome::next(
                    json!({"status":"error","msg":"PowerShell interpreter not found. Install pwsh/powershell or use code_run type=bash."}),
                    "\n",
                ));
            };
            let mut c = Command::new(pwsh);
            c.arg("-NoProfile").arg("-Command").arg(code);
            c
        } else {
            return Ok(StepOutcome::next(
                json!({"status":"error","msg":format!("不支持的类型: {typ}")}),
                "\n",
            ));
        };
        cmd.current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let stop_path = self.cfg.temp_dir.join("_stop_signal");
        let (exit_code, status_str, stdout) =
            run_command_collecting_output(cmd, Duration::from_secs(secs), &stop_path).await?;
        if let Some(path) = tmp_path {
            let _ = fs::remove_file(path);
        }
        Ok(StepOutcome::next(
            json!({"status": status_str, "stdout": smart_format(&stdout, 10000, "\n\n[omitted long output]\n\n"), "exit_code": exit_code}),
            anchor_prompt(index),
        ))
    }

    fn do_file_read(&self, args: Value) -> Result<StepOutcome> {
        let path = self.abs(
            args.get("path")
                .and_then(Value::as_str)
                .context("path missing")?,
        );
        let start = args
            .get("start")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let count = args.get("count").and_then(Value::as_u64).unwrap_or(200) as usize;
        let keyword = args
            .get("keyword")
            .and_then(Value::as_str)
            .map(str::to_lowercase);
        let show_linenos = args
            .get("show_linenos")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                return Ok(StepOutcome::next(
                    self.format_file_not_found_with_suggestions(&path),
                    "\n",
                ));
            }
        };
        self.remember_read_dir(&path);
        let lines: Vec<_> = text.lines().map(str::to_string).collect();
        let mut begin = start.saturating_sub(1);
        if let Some(k) = keyword {
            if let Some(pos) = lines
                .iter()
                .enumerate()
                .skip(begin)
                .find(|(_, l)| l.to_lowercase().contains(&k))
                .map(|(i, _)| i)
            {
                begin = pos.saturating_sub(count / 3);
            } else {
                return Ok(StepOutcome::next(
                    format!(
                        "Keyword '{k}' not found after line {start}. Falling back to content from line {start}:\n\n{}",
                        render_file_lines(
                            &lines,
                            begin,
                            (begin + count).min(lines.len()),
                            show_linenos
                        )
                    ),
                    "\n",
                ));
            }
        }
        let end = (begin + count).min(lines.len());
        let mut result = render_file_lines(&lines, begin, end, show_linenos);
        if show_linenos && !result.starts_with("Error:") {
            result = format!("由于设置了show_linenos，以下返回信息为：(行号|)内容 。\n{result}");
        }
        if result.contains(" ... [TRUNCATED]") {
            result.push_str("\n\n（某些行被截断，如需完整内容可改用 code_run 读取）");
        }
        log_memory_access(&self.cfg, &path);
        let mut next_prompt = "\n".to_string();
        let path_s = path.to_string_lossy().to_ascii_lowercase();
        if path_s.contains("memory") || path_s.contains("sop") {
            next_prompt.push_str("\n[SYSTEM TIPS] 正在读取记忆或SOP文件，若决定按sop执行请提取sop中的关键点（特别是靠后的）update working memory.");
        }
        Ok(StepOutcome::next(
            smart_format(&result, 20000, "\n\n[omitted long content]\n\n"),
            next_prompt,
        ))
    }

    fn remember_read_dir(&self, path: &Path) {
        if let Some(parent) = path.parent()
            && let Ok(mut dirs) = self.read_dirs.lock()
        {
            let parent = parent.to_path_buf();
            if !dirs.iter().any(|p| p == &parent) {
                dirs.push(parent);
            }
        }
    }

    fn format_file_not_found_with_suggestions(&self, path: &Path) -> String {
        let mut msg = format_file_not_found(path);
        let target = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if target.is_empty() {
            return msg;
        }
        let mut roots = vec![path.parent().unwrap_or(&self.cwd).to_path_buf()];
        if !roots.iter().any(|p| p == &self.cwd) {
            roots.push(self.cwd.clone());
        }
        if let Ok(dirs) = self.read_dirs.lock() {
            for dir in dirs.iter() {
                if !roots.iter().any(|p| p == dir) {
                    roots.push(dir.clone());
                }
            }
        }
        let mut scored = Vec::new();
        for root in roots {
            for entry in WalkDir::new(root)
                .max_depth(4)
                .into_iter()
                .flatten()
                .take(2000)
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                let score = filename_similarity(&target, &name);
                if score > 0.3 {
                    scored.push((score, entry.path().display().to_string()));
                }
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.dedup_by(|a, b| a.1 == b.1);
        if !scored.is_empty() {
            msg.push_str("\n\nDid you mean:\n");
            for (score, cand) in scored.into_iter().take(5) {
                msg.push_str(&format!("  {cand}  ({:.0}%)\n", score * 100.0));
            }
        }
        msg.trim_end().to_string()
    }

    fn do_file_patch(&self, args: Value) -> Result<StepOutcome> {
        let path = self.abs(
            args.get("path")
                .and_then(Value::as_str)
                .context("path missing")?,
        );
        let old = args
            .get("old_content")
            .and_then(Value::as_str)
            .context("old_content missing")?;
        let new = args
            .get("new_content")
            .and_then(Value::as_str)
            .context("new_content missing")?;
        let new = expand_file_refs(new, &self.cwd)?;
        if old.is_empty() {
            return Ok(StepOutcome::next(
                json!({"status":"error","msg":"old_content 为空，请确认 arguments"}),
                "\n",
            ));
        }
        let full = match fs::read_to_string(&path) {
            Ok(full) => full,
            Err(_) => {
                return Ok(StepOutcome::next(
                    json!({"status":"error","msg":"文件不存在"}),
                    "\n",
                ));
            }
        };
        let count = full.matches(old).count();
        let ret = if count == 0 {
            json!({"status":"error","msg":"未找到匹配的旧文本块，建议：先用 file_read 确认当前内容，再分小段进行 patch。若多次失败则询问用户，严禁自行使用 overwrite 或代码替换。"})
        } else if count > 1 {
            json!({"status":"error","msg":format!("找到 {count} 处匹配，无法确定唯一位置。请提供更长、更具体的旧文本块以确保唯一性。建议：包含上下文行来增强特征，或分小段逐个修改。")})
        } else {
            fs::write(&path, full.replace(old, &new))?;
            json!({"status":"success","msg":"文件局部修改成功"})
        };
        Ok(StepOutcome::next(ret, "\n"))
    }

    fn do_file_write(&self, args: Value, response: &AgentResponse) -> Result<StepOutcome> {
        let path = self.write_abs(args.get("path").and_then(Value::as_str));
        let mode = args
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("overwrite");
        let mut content = args
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if content.is_empty() {
            content = extract_file_content(&response.content).unwrap_or_default();
        }
        if content.is_empty() {
            return Ok(StepOutcome::next(
                json!({"status":"error","msg":"No content found. Blank is not supported. Put content inside <file_content>...</file_content> tags in your reply body before call file_write."}),
                "\n",
            ));
        }
        content = expand_file_refs(&content, &self.cwd)?;
        let write_result = (|| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            match mode {
                "prepend" => {
                    let old = fs::read_to_string(&path).unwrap_or_default();
                    fs::write(&path, format!("{content}{old}"))?;
                }
                "append" => {
                    use std::io::Write;
                    fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)?
                        .write_all(content.as_bytes())?;
                }
                _ => fs::write(&path, &content)?,
            }
            Ok(())
        })();
        if let Err(e) = write_result {
            return Ok(StepOutcome::next(
                json!({"status":"error","msg":format!("写入异常: {e}"),"path":path.display().to_string()}),
                "\n",
            ));
        }
        Ok(StepOutcome::next(
            json!({"status":"success","writed_bytes":content.len(),"path":path.display().to_string()}),
            "\n",
        ))
    }
}

fn render_file_lines(lines: &[String], begin: usize, end: usize, show_linenos: bool) -> String {
    let begin = begin.min(lines.len());
    let end = end.min(lines.len()).max(begin);
    let mut result = String::new();
    if show_linenos {
        result.push_str(&format!(
            "[FILE] {} lines{}\n",
            lines.len(),
            if end < lines.len() {
                format!(" | PARTIAL showing {}", end - begin)
            } else {
                String::new()
            }
        ));
    }
    for (i, line) in lines[begin..end].iter().enumerate() {
        if show_linenos {
            result.push_str(&format!("{}|{}\n", begin + i + 1, truncate_line(line)));
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    result.trim_end().to_string()
}

impl GenericToolDispatcher {
    async fn do_web_scan(&self, args: Value) -> Result<StepOutcome> {
        let tabs_only = args
            .get("tabs_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let switch_tab_id = args
            .get("switch_tab_id")
            .or_else(|| args.get("tab_id"))
            .and_then(Value::as_str);
        let text_only = args
            .get("text_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let cutlist = args
            .get("cutlist")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let instruction = args
            .get("instruction")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Ok(tabs) = cdp_tabs().await else {
            return Ok(StepOutcome::next(
                json!({"status":"error","msg":"没有可用的浏览器标签页，查L3记忆分析原因。"}),
                "\n",
            ));
        };
        let active_index = select_tab_index(&tabs, switch_tab_id);
        let active_tab = active_index
            .and_then(|i| tabs.get(i))
            .cloned()
            .unwrap_or(Value::Null);
        let content = if tabs_only {
            None
        } else if let Some(ws) = active_tab
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
        {
            let expr = self.browser_extract_expr(text_only, cutlist, instruction);
            match cdp_eval(ws, &expr, false).await {
                Ok(v) => Some(simplify_browser_content(&v, text_only)),
                Err(e) => Some(format!("[web_scan error] {e:#}")),
            }
        } else {
            Some("No CDP websocket URL for active tab.".to_string())
        };
        let result = json!({
            "status":"success",
            "metadata":{"tabs_count":tabs.len(),"tabs":tabs,"active_tab":active_tab.get("id").cloned().unwrap_or(Value::Null)},
            "content": content
        });
        Ok(StepOutcome::next(result, "\n"))
    }

    async fn do_web_execute_js(
        &self,
        args: Value,
        response: &AgentResponse,
    ) -> Result<StepOutcome> {
        let Some(script) = self.resolve_js_script(&args, response) else {
            return Ok(StepOutcome::next(
                json!({"status":"error","msg":"Script missing. Use javascript block or 'script' arg."}),
                "\n",
            ));
        };
        let switch_tab_id = args
            .get("switch_tab_id")
            .or_else(|| args.get("tab_id"))
            .and_then(Value::as_str);
        let no_monitor = args
            .get("no_monitor")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let save_to_file = args.get("save_to_file").and_then(Value::as_str);
        if let Some(bridge_cmd) = parse_tmwd_bridge_command(&script) {
            return self
                .do_tmwd_bridge_command(bridge_cmd, switch_tab_id, save_to_file)
                .await;
        }
        let Ok(tabs) = cdp_tabs().await else {
            return Ok(StepOutcome::next(
                json!({"status":"error","msg":"没有可用的浏览器标签页，查L3记忆分析原因。"}),
                "\n",
            ));
        };
        let before_tab_ids = tab_ids(&tabs);
        let active = select_tab_index(&tabs, switch_tab_id)
            .and_then(|i| tabs.get(i))
            .cloned()
            .unwrap_or(Value::Null);
        let tab_id = active.get("id").and_then(Value::as_str).map(str::to_string);
        let before_url = active
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(ws) = active.get("webSocketDebuggerUrl").and_then(Value::as_str) else {
            return Ok(StepOutcome::next(
                json!({"status":"error","msg":"active tab has no webSocketDebuggerUrl"}),
                "\n",
            ));
        };
        let before = if no_monitor {
            None
        } else {
            let expr = self.browser_extract_expr(false, false, "");
            let _ = cdp_eval(ws, TEMP_MONITOR_JS, false).await;
            cdp_eval(ws, &expr, false)
                .await
                .ok()
                .map(|v| simplify_browser_content(&v, false))
        };
        let (js_return, error_msg) = match cdp_eval(ws, &script, true).await {
            Ok(v) => (v, None),
            Err(e) => (Value::Null, Some(format!("{e:#}"))),
        };
        sleep(Duration::from_millis(800)).await;
        if let Some(path) = save_to_file {
            let abs = self.abs(path);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&abs, value_to_string(&js_return))?;
        }
        let after_tabs = cdp_tabs().await.unwrap_or_default();
        let new_tabs = new_tabs_since(&before_tab_ids, &after_tabs);
        let active_after = tab_id.as_deref().and_then(|id| {
            after_tabs
                .iter()
                .find(|t| t.get("id").and_then(Value::as_str) == Some(id))
        });
        let after_url = active_after
            .and_then(|t| t.get("url").and_then(Value::as_str))
            .map(str::to_string);
        let reloaded = tab_id.as_deref().is_some_and(|_| active_after.is_none())
            || before_url
                .as_deref()
                .zip(after_url.as_deref())
                .is_some_and(|(before, after)| !before.is_empty() && before != after);
        let transients = if no_monitor || reloaded {
            Vec::new()
        } else {
            cdp_eval(ws, TEMP_MONITOR_STOP_JS, false)
                .await
                .ok()
                .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
                .unwrap_or_default()
        };
        let after = if no_monitor || reloaded {
            None
        } else {
            let expr = self.browser_extract_expr(false, false, "");
            cdp_eval(ws, &expr, false)
                .await
                .ok()
                .map(|v| simplify_browser_content(&v, false))
        };
        let (diff, changed) = before.zip(after).map_or((None, None), |(b, a)| {
            if b == a {
                (Some(dom_diff_summary(&b, &a)), Some(String::new()))
            } else {
                (
                    Some(dom_diff_summary(&b, &a)),
                    Some(smart_format(&a, 6000, "\n\n[omitted page html]\n\n")),
                )
            }
        });
        let suggestion =
            web_execute_suggestion(reloaded, !new_tabs.is_empty(), diff.as_deref(), &transients);
        Ok(StepOutcome::next(
            json!({
                "status": if error_msg.is_some() { "failed" } else { "success" },
                "js_return":js_return,
                "tab_id":tab_id,
                "reloaded":reloaded,
                "newTabs":new_tabs,
                "transients":transients,
                "page_changed_text":changed,
                "diff":diff,
                "suggestion":suggestion,
                "error":error_msg
            }),
            "\n",
        ))
    }

    async fn do_tmwd_bridge_command(
        &self,
        mut cmd: Value,
        switch_tab_id: Option<&str>,
        save_to_file: Option<&str>,
    ) -> Result<StepOutcome> {
        if cmd.get("tabId").is_none()
            && let Some(tab_id) = switch_tab_id
            && let Some(obj) = cmd.as_object_mut()
        {
            obj.insert("tabId".into(), Value::String(tab_id.to_string()));
        }
        let result = match execute_tmwd_bridge_command(&cmd).await {
            Ok(v) => json!({
                "status":"success",
                "js_return": v,
                "tab_id": cmd.get("tabId").cloned().unwrap_or(Value::Null),
                "bridge":"cdp-compatible"
            }),
            Err(e) => json!({
                "status":"failed",
                "error": format!("{e:#}"),
                "hint":"tmwd_cdp_bridge JSON commands require either Chrome remote debugging on 127.0.0.1:9222 or the original TMWebDriver extension master.",
                "bridge":"cdp-compatible"
            }),
        };
        if let (Some(path), Some(value)) = (save_to_file, result.get("js_return")) {
            let abs = self.abs(path);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&abs, value_to_string(value))?;
        }
        Ok(StepOutcome::next(result, "\n"))
    }

    fn do_ask_user(&self, args: Value) -> StepOutcome {
        StepOutcome::exit(
            json!({"status":"INTERRUPT","intent":"HUMAN_INTERVENTION","data":{"question":args.get("question").cloned().unwrap_or(json!("请提供输入：")),"candidates":args.get("candidates").cloned().unwrap_or(json!([]))}}),
        )
    }

    fn resolve_js_script(&self, args: &Value, response: &AgentResponse) -> Option<String> {
        let script = args
            .get("script")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| extract_code_block(&response.content, "javascript"))?;
        let trimmed = script.trim();
        if trimmed.lines().count() == 1 {
            let path = self.abs(trimmed);
            if path.is_file()
                && let Ok(content) = fs::read_to_string(path)
            {
                return Some(content);
            }
        }
        Some(script)
    }

    fn with_code_run_header(&self, code: &str) -> String {
        let header = self.cfg.resource_dir.join("assets/code_run_header.py");
        let bootstrap = format!(
            "import os, sys\nos.environ.setdefault('KODA_AGENT_HOME', {home:?})\nos.environ.setdefault('KODA_AGENT_ROOT', {resource:?})\nos.environ.setdefault('KODA_WORKSPACE', {workspace:?})\nos.environ.setdefault('KODA_MEMORY_DIR', {memory:?})\nfor _koda_p in [{memory:?}, os.path.join({resource:?}, 'memory')]:\n    if _koda_p and _koda_p not in sys.path: sys.path.insert(0, _koda_p)\n",
            home = self.cfg.home_dir.display().to_string(),
            resource = self.cfg.resource_dir.display().to_string(),
            workspace = self.cfg.workspace_dir.display().to_string(),
            memory = self.cfg.memory_dir.display().to_string()
        );
        match fs::read_to_string(header) {
            Ok(header) => format!("{bootstrap}{header}\n{code}"),
            Err(_) => format!("{bootstrap}{code}"),
        }
    }

    fn browser_extract_expr(&self, text_only: bool, cutlist: bool, instruction: &str) -> String {
        let opt = self.cfg.resource_dir.join("assets/simphtml_opt.js");
        match fs::read_to_string(opt) {
            Ok(js) if cutlist && !text_only => {
                let list_js = fs::read_to_string(
                    self.cfg
                        .resource_dir
                        .join("assets/simphtml_find_main_list.js"),
                )
                .unwrap_or_default();
                format!("{js}\n{list_js}\n{}", cutlist_browser_expr(instruction))
            }
            Ok(js) => format!(
                "{js}\noptHTML({});",
                if text_only { "true" } else { "false" }
            ),
            Err(_) if text_only => {
                "document.body ? document.body.innerText : document.documentElement.innerText"
                    .to_string()
            }
            Err(_) if cutlist => cutlist_browser_expr(instruction),
            Err(_) => "document.documentElement.outerHTML".to_string(),
        }
    }

    fn handle_inline_eval_control(&self, code: &str, index: usize) -> Result<Option<StepOutcome>> {
        if let Some(plan) = extract_call_arg(code, "enter_plan_mode") {
            fs::create_dir_all(&self.cfg.temp_dir)?;
            fs::write(self.cfg.temp_dir.join("_plan_mode"), &plan)?;
            return Ok(Some(StepOutcome::next(
                json!({"status":"success","stdout":format!("Entered plan mode with plan file: {plan}"),"exit_code":0}),
                anchor_prompt(index),
            )));
        }
        if let Some(hook) = extract_append_arg(code, "_done_hooks.append") {
            fs::create_dir_all(&self.cfg.temp_dir)?;
            use std::io::Write;
            writeln!(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.cfg.temp_dir.join("_done_hooks"))?,
                "{hook}"
            )?;
            return Ok(Some(StepOutcome::next(
                json!({"status":"success","stdout":format!("Registered done hook: {hook}"),"exit_code":0}),
                anchor_prompt(index),
            )));
        }
        if code.contains("_exit_plan_mode") || code.contains("exit_plan_mode") {
            fs::create_dir_all(&self.cfg.temp_dir)?;
            fs::write(self.cfg.temp_dir.join("_exit_plan_mode"), "1")?;
            return Ok(Some(StepOutcome::next(
                json!({"status":"success","stdout":"Exited plan mode","exit_code":0}),
                anchor_prompt(index),
            )));
        }
        Ok(None)
    }

    fn do_update_working_checkpoint(&self, args: Value) -> Result<StepOutcome> {
        fs::create_dir_all(&self.cfg.temp_dir)?;
        fs::write(
            self.cfg.temp_dir.join("working_checkpoint.json"),
            serde_json::to_vec_pretty(&args)?,
        )?;
        if let Some(key_info) = args.get("key_info").and_then(Value::as_str) {
            fs::write(self.cfg.temp_dir.join("_keyinfo"), key_info)?;
        }
        if let Some(related_sop) = args.get("related_sop").and_then(Value::as_str)
            && !related_sop.trim().is_empty()
        {
            fs::write(
                self.cfg.temp_dir.join("_related_sop"),
                format!("有不清晰的地方请再次读取{related_sop}"),
            )?;
        }
        Ok(StepOutcome::next(
            json!({"result":"working key_info updated","status":"success","msg":"working checkpoint updated"}),
            "\n",
        ))
    }

    fn do_start_long_term_update(&self, args: Value) -> Result<StepOutcome> {
        fs::create_dir_all(&self.cfg.memory_dir)?;
        let path = self.cfg.memory_dir.join("long_term_updates.jsonl");
        use std::io::Write;
        writeln!(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?,
            "{}",
            serde_json::to_string(&args)?
        )?;
        let l0 = self.cfg.memory_dir.join("memory_management_sop.md");
        let result = if l0.exists() {
            format!(
                "This is L0:\n{}",
                fs::read_to_string(l0).unwrap_or_else(|_| {
                    "Memory Management SOP could not be read. Do not update memory.".into()
                })
            )
        } else {
            "Memory Management SOP not found. Do not update memory.".into()
        };
        let prompt = format!(
            "### [总结提炼经验] 既然你觉得当前任务有重要信息需要记忆，请提取最近一次任务中【事实验证成功且长期有效】的环境事实、用户偏好、重要步骤，更新记忆。\n\
本工具是标记开启结算过程，若已在更新记忆过程或没有值得记忆的点，忽略本次调用。\n\
**如果没有经验证的，未来能用上的信息，忽略本次调用！**\n\
**只能提取行动验证成功的信息**：\n\
- **首选结构化结算**：调用本工具时使用 {{\"l2\":{{\"section\":\"Topic\",\"facts\":[\"经工具验证的长期事实\"]}},\"l3\":{{\"file\":\"topic_sop.md\",\"content\":\"极短可复用经验\"}}}}；native settle 会自动同步短 L1 索引\n\
- **环境事实**（路径/配置/稳定约束）→ L2；**复杂任务经验**（关键坑点/前置条件/重要步骤）→ L3 精简 SOP\n\
**禁止**：临时变量、具体推理过程、未验证信息、通用常识、你可以轻松复现的细节、只是做了但没有验证的信息\n\
**操作**：严格遵循提供的L0的记忆更新SOP。先 `file_read` 看现有 → 判断类型 → 最小化更新/结构化排队 → 无新内容跳过，保证对记忆库最小局部修改。\n\n{}",
            global_memory_prompt_for_tools(&self.cfg)
        );
        Ok(StepOutcome::next(result, prompt))
    }
}

fn anchor_prompt(index: usize) -> String {
    if index > 0 {
        String::new()
    } else {
        "\n".into()
    }
}

async fn run_command_collecting_output(
    mut cmd: Command,
    timeout_duration: Duration,
    stop_path: &Path,
) -> Result<(Option<i32>, &'static str, String)> {
    let mut child = cmd.spawn()?;
    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::default();
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::default();
    let stdout_task = child
        .stdout
        .take()
        .map(|out| spawn_output_reader(out, Arc::clone(&stdout_buf)));
    let stderr_task = child
        .stderr
        .take()
        .map(|err| spawn_output_reader(err, Arc::clone(&stderr_buf)));

    let started = Instant::now();
    let mut killed_msg = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if stop_path.exists() {
            killed_msg = Some("\n[Stopped] 用户强制终止");
            let _ = child.kill().await;
            break child.wait().await?;
        }
        if started.elapsed() > timeout_duration {
            killed_msg = Some("\n[Timeout Error] 超时强制终止");
            let _ = child.kill().await;
            break child.wait().await?;
        }
        sleep(Duration::from_millis(200)).await;
    };

    if let Some(task) = stdout_task {
        let _ = timeout(Duration::from_secs(1), task).await;
    }
    if let Some(task) = stderr_task {
        let _ = timeout(Duration::from_secs(1), task).await;
    }
    let mut combined = stdout_buf.lock().unwrap().clone();
    combined.extend_from_slice(&stderr_buf.lock().unwrap());
    let mut stdout = String::from_utf8_lossy(&combined).to_string();
    if let Some(msg) = killed_msg {
        stdout.push_str(msg);
    }
    let status_str = if status.success() { "success" } else { "error" };
    Ok((status.code(), status_str, stdout))
}

fn spawn_output_reader<R>(mut reader: R, sink: Arc<Mutex<Vec<u8>>>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
            }
        }
    })
}

fn find_program(candidates: &[&str]) -> Option<String> {
    candidates.iter().find_map(|candidate| {
        std::process::Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .filter(|s| s.success())
            .map(|_| (*candidate).to_string())
    })
}

fn extract_code_block(content: &str, typ: &str) -> Option<String> {
    let aliases: Vec<&str> = match typ {
        "python" | "py" => vec!["python", "py"],
        "bash" | "sh" | "shell" => vec!["bash", "sh", "shell"],
        "powershell" | "ps1" | "pwsh" => vec!["powershell", "ps1", "pwsh"],
        other => vec![other],
    };
    let mut rest = content;
    let mut found = None;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let Some(nl) = after.find('\n') else { break };
        let lang = after[..nl].trim().to_ascii_lowercase();
        let body = &after[nl + 1..];
        let Some(end) = body.find("```") else { break };
        if aliases.iter().any(|a| *a == lang) {
            found = Some(body[..end].trim().to_string());
        }
        rest = &body[end + 3..];
    }
    found
}

fn inline_eval_wrapper(code: &str) -> String {
    format!(
        r#"__koda_code = {code:?}
__koda_ns = {{}}
try:
    try:
        __koda_result = eval(__koda_code, __koda_ns)
    except SyntaxError:
        exec(__koda_code, __koda_ns)
        __koda_result = __koda_ns.get("_r", "OK")
    print(repr(__koda_result))
except Exception as __koda_error:
    print("Error: " + str(__koda_error))
"#
    )
}

fn extract_call_arg(code: &str, name: &str) -> Option<String> {
    let start = code.find(name)?;
    let after_name = &code[start + name.len()..];
    let open = after_name.find('(')? + start + name.len();
    extract_first_string_arg(&code[open + 1..])
}

fn extract_append_arg(code: &str, name: &str) -> Option<String> {
    extract_call_arg(code, name)
}

fn extract_first_string_arg(args: &str) -> Option<String> {
    let mut chars = args.char_indices().peekable();
    while let Some((_, ch)) = chars.peek().copied() {
        if ch.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    let (_, quote) = chars.next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for (_, ch) in chars {
        if escaped {
            out.push(match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

fn filename_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if b.contains(a) || a.contains(b) {
        return 0.8;
    }
    let la = a.chars().count();
    let lb = b.chars().count();
    if la == 0 || lb == 0 {
        return 0.0;
    }
    let lcs = lcs_len(a, b);
    (2 * lcs) as f64 / (la + lb) as f64
}

fn lcs_len(a: &str, b: &str) -> usize {
    let aa = a.chars().collect::<Vec<_>>();
    let bb = b.chars().collect::<Vec<_>>();
    let mut prev = vec![0; bb.len() + 1];
    let mut cur = vec![0; bb.len() + 1];
    for ca in &aa {
        for (j, cb) in bb.iter().enumerate() {
            cur[j + 1] = if ca == cb {
                prev[j] + 1
            } else {
                cur[j].max(prev[j + 1])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
        cur.fill(0);
    }
    prev[bb.len()]
}

fn truncate_line(line: &str) -> String {
    const L: usize = 8000;
    if line.chars().count() <= L {
        line.into()
    } else {
        format!(
            "{} ... [TRUNCATED]",
            line.chars().take(L).collect::<String>()
        )
    }
}
fn format_file_not_found(path: &Path) -> String {
    format!("Error: File not found: {}", path.display())
}

async fn cdp_tabs() -> Result<Vec<Value>> {
    let resp = reqwest::get("http://127.0.0.1:9222/json").await?;
    let tabs: Vec<Value> = resp.json().await?;
    Ok(tabs
        .into_iter()
        .filter(|t| t.get("type").and_then(Value::as_str).unwrap_or("page") == "page")
        .collect())
}

fn select_tab_index(tabs: &[Value], requested: Option<&str>) -> Option<usize> {
    if tabs.is_empty() {
        return None;
    }
    if let Some(req) = requested {
        if let Ok(i) = req.parse::<usize>()
            && i < tabs.len()
        {
            return Some(i);
        }
        if let Some((i, _)) = tabs
            .iter()
            .enumerate()
            .find(|(_, t)| t.get("id").and_then(Value::as_str) == Some(req))
        {
            return Some(i);
        }
    }
    tabs.iter()
        .position(|t| {
            t.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| !u.starts_with("devtools://"))
        })
        .or(Some(0))
}

fn tab_ids(tabs: &[Value]) -> Vec<String> {
    tabs.iter()
        .filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn new_tabs_since(before_ids: &[String], after_tabs: &[Value]) -> Vec<Value> {
    after_tabs
        .iter()
        .filter(|tab| {
            tab.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !before_ids.iter().any(|old| old == id))
        })
        .map(|tab| {
            json!({
                "id": tab.get("id").cloned().unwrap_or(Value::Null),
                "url": tab.get("url").cloned().unwrap_or(Value::Null),
                "title": tab.get("title").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

fn parse_tmwd_bridge_command(script: &str) -> Option<Value> {
    let trimmed = script.trim();
    if !(trimmed.starts_with('{') && trimmed.ends_with('}')) {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    let is_bridge = value
        .get("cmd")
        .and_then(Value::as_str)
        .filter(|cmd| {
            matches!(
                *cmd,
                "tabs" | "cookies" | "cdp" | "batch" | "management" | "contentSettings"
            )
        })
        .is_some();
    is_bridge.then_some(value)
}

async fn execute_tmwd_bridge_command(cmd: &Value) -> Result<Value> {
    if let Ok(v) = execute_tmwd_master_command(cmd).await {
        return Ok(v);
    }
    match cmd.get("cmd").and_then(Value::as_str).unwrap_or_default() {
        "tabs" => execute_bridge_tabs(cmd).await,
        "cookies" => execute_bridge_cookies(cmd).await,
        "cdp" => execute_bridge_cdp(cmd).await,
        "batch" => execute_bridge_batch(cmd).await,
        "management" | "contentSettings" => {
            bail!(
                "cmd={} needs the installed tmwd_cdp_bridge extension; CDP fallback cannot access Chrome extension APIs",
                cmd.get("cmd").and_then(Value::as_str).unwrap_or_default()
            )
        }
        other => bail!("unknown tmwd bridge cmd: {other}"),
    }
}

async fn execute_tmwd_master_command(cmd: &Value) -> Result<Value> {
    let body = json!({"cmd":"execute_js","code":cmd,"timeout":15});
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(700))
        .timeout(Duration::from_secs(20))
        .build()?;
    let response = client
        .post("http://127.0.0.1:18766/link")
        .json(&body)
        .send()
        .await?;
    let value: Value = response.json().await?;
    let result = value.get("r").cloned().unwrap_or(Value::Null);
    if result.get("error").is_some() {
        bail!("{}", tmwd_error_message(&result));
    }
    Ok(result.get("data").cloned().unwrap_or(result))
}

fn tmwd_error_message(result: &Value) -> String {
    let Some(error) = result.get("error") else {
        return "tmwebdriver master error".to_string();
    };
    error
        .get("message")
        .or_else(|| error.get("msg"))
        .map(value_to_string)
        .unwrap_or_else(|| value_to_string(error))
}

async fn execute_bridge_tabs(cmd: &Value) -> Result<Value> {
    if cmd.get("method").and_then(Value::as_str) == Some("switch") {
        let id = cmd
            .get("tabId")
            .and_then(value_as_id)
            .context("tabs switch requires tabId")?;
        let url = format!("http://127.0.0.1:9222/json/activate/{id}");
        let body = reqwest::get(url).await?.text().await?;
        return Ok(json!({"ok":true,"data":body}));
    }
    let tabs = cdp_tabs().await?;
    Ok(json!({
        "ok": true,
        "data": tabs.into_iter().map(|t| json!({
            "id": t.get("id").cloned().unwrap_or(Value::Null),
            "url": t.get("url").cloned().unwrap_or(Value::Null),
            "title": t.get("title").cloned().unwrap_or(Value::Null),
            "active": false,
            "windowId": Value::Null
        })).collect::<Vec<_>>()
    }))
}

async fn execute_bridge_cookies(cmd: &Value) -> Result<Value> {
    let tabs = cdp_tabs().await?;
    let tab = select_cdp_tab(&tabs, cmd.get("tabId").and_then(value_as_id).as_deref())?;
    let ws = tab
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .context("active tab has no webSocketDebuggerUrl")?;
    let url = cmd
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| tab.get("url").and_then(Value::as_str).map(str::to_string))
        .context("cookies command requires url or active tab url")?;
    let data = cdp_call(ws, "Network.getCookies", json!({"urls":[url]})).await?;
    Ok(json!({"ok":true,"data":data.get("cookies").cloned().unwrap_or(json!([]))}))
}

async fn execute_bridge_cdp(cmd: &Value) -> Result<Value> {
    let tabs = cdp_tabs().await?;
    let tab = select_cdp_tab(&tabs, cmd.get("tabId").and_then(value_as_id).as_deref())?;
    let ws = tab
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .context("active tab has no webSocketDebuggerUrl")?;
    let method = cmd
        .get("method")
        .and_then(Value::as_str)
        .context("cdp command requires method")?;
    let params = cmd.get("params").cloned().unwrap_or_else(|| json!({}));
    let data = cdp_call(ws, method, params).await?;
    Ok(json!({"ok":true,"data":data}))
}

async fn execute_bridge_batch(cmd: &Value) -> Result<Value> {
    let commands = cmd
        .get("commands")
        .and_then(Value::as_array)
        .context("batch command requires commands array")?;
    let mut results = Vec::with_capacity(commands.len());
    for item in commands {
        let mut child = item.clone();
        child = resolve_batch_refs(child, &results);
        if child.get("tabId").is_none()
            && let Some(tab_id) = cmd.get("tabId").cloned()
            && let Some(obj) = child.as_object_mut()
        {
            obj.insert("tabId".into(), tab_id);
        }
        let child_result = match child.get("cmd").and_then(Value::as_str).unwrap_or_default() {
            "tabs" => execute_bridge_tabs(&child).await,
            "cookies" => execute_bridge_cookies(&child).await,
            // Upstream handleBatch pushes raw chrome.debugger.sendCommand output for CDP,
            // while standalone {cmd:"cdp"} wraps it as {ok:true,data:...}.
            "cdp" => execute_bridge_cdp(&child)
                .await
                .map(|v| v.get("data").cloned().unwrap_or(v)),
            "batch" => Err(anyhow!("nested tmwd batch commands are not supported")),
            "management" | "contentSettings" => Err(anyhow!(
                "cmd={} needs the installed tmwd_cdp_bridge extension; CDP fallback cannot access Chrome extension APIs",
                child.get("cmd").and_then(Value::as_str).unwrap_or_default()
            )),
            other => Err(anyhow!("unknown tmwd bridge cmd: {other}")),
        };
        match child_result {
            Ok(v) => results.push(v),
            Err(e) => results.push(json!({"ok":false,"error":format!("{e:#}")})),
        }
    }
    Ok(json!({"ok":true,"results":results}))
}

fn resolve_batch_refs(value: Value, results: &[Value]) -> Value {
    match value {
        Value::String(s) => resolve_batch_ref_string(&s, results).unwrap_or(Value::String(s)),
        Value::Array(arr) => Value::Array(
            arr.into_iter()
                .map(|v| resolve_batch_refs(v, results))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, resolve_batch_refs(v, results)))
                .collect(),
        ),
        other => other,
    }
}

fn resolve_batch_ref_string(s: &str, results: &[Value]) -> Option<Value> {
    let rest = s.strip_prefix('$')?;
    let (idx, path) = rest.split_once('.')?;
    let mut value = results.get(idx.parse::<usize>().ok()?)?;
    for part in path.split('.') {
        if part.is_empty() {
            return None;
        }
        value = match value {
            Value::Array(arr) => arr.get(part.parse::<usize>().ok()?)?,
            Value::Object(map) => map.get(part)?,
            _ => return None,
        };
    }
    Some(value.clone())
}

fn select_cdp_tab<'a>(tabs: &'a [Value], requested: Option<&str>) -> Result<&'a Value> {
    select_tab_index(tabs, requested)
        .and_then(|i| tabs.get(i))
        .context("no available browser tab")
}

fn value_as_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|v| v.to_string()))
        .or_else(|| value.as_u64().map(|v| v.to_string()))
}

fn cutlist_browser_expr(instruction: &str) -> String {
    let instruction = serde_json::to_string(instruction).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"
(function kodaCutlistHTML() {{
    const instruction = {instruction};
    const baseHTML = (typeof optHTML === 'function') ? optHTML(false) : document.documentElement.outerHTML;
    if (typeof findMainList !== 'function') return baseHTML;
    let lists = [];
    try {{ lists = findMainList(document.body); }} catch (_) {{ return baseHTML; }}
    if (!Array.isArray(lists) || !lists.length) return baseHTML;
    const root = document.documentElement.cloneNode(true);
    const doc = document.implementation.createHTMLDocument('');
    doc.documentElement.replaceWith(root);
    let hiddenGroups = 0;
    for (const entry of lists) {{
        const sel = entry && entry.selector;
        if (!sel) continue;
        let items = [];
        try {{ items = Array.from(root.querySelectorAll(sel)); }} catch (_) {{ continue; }}
        if (items.length < 5) continue;
        const totalLen = items.reduce((n, it) => n + it.outerHTML.length, 0);
        const avgLen = totalLen / items.length;
        if (avgLen < 200 || (avgLen < 700 && totalLen < 2500)) continue;
        const hits = instruction ? items.filter(it => (it.textContent || '').includes(instruction)).slice(0, 6) : [];
        const keep = hits.length ? hits : items.slice(0, 3);
        const keepSet = new Set(keep);
        const removed = items.filter(it => !keepSet.has(it));
        if (!removed.length) continue;
        const sampleTexts = removed.slice(0, 5).map(it => (it.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 40)).filter(Boolean);
        const hint = doc.createElement('div');
        hint.textContent = `[FAKE ELEMENT] ${{removed.length}} more items hidden, selector: "${{sel}}"` + (sampleTexts.length ? ` Hidden items: "${{sampleTexts.join('","')}}"` : '');
        keep[keep.length - 1].insertAdjacentElement('afterend', hint);
        removed.forEach(it => it.remove());
        hiddenGroups++;
    }}
    return hiddenGroups ? root.outerHTML : baseHTML;
}})();
"#
    )
}

const TEMP_MONITOR_JS: &str = r#"
(function startStrMonitor(interval) {
    if (window._tm && window._tm.id) clearInterval(window._tm.id);
    window._tm = {extract: () => {
        const texts = new Set(), walker = document.createTreeWalker(document.body || document.documentElement, NodeFilter.SHOW_TEXT);
        let node, t, s;
        while (node = walker.nextNode()) {
            t = (node.textContent || '').trim();
            s = t.substring(0, 20);
            if (t && t.length > 10 && !s.includes('_')) texts.add(s);
        }
        return texts;
    }};
    window._tm.init = window._tm.extract();
    window._tm.all = new Set();
    window._tm.id = setInterval(() => window._tm.extract().forEach(t => window._tm.all.add(t)), interval || 450);
})(450);
"#;

const TEMP_MONITOR_STOP_JS: &str = r#"
(function stopStrMonitor() {
    if (!window._tm) return [];
    clearInterval(window._tm.id);
    const final = window._tm.extract();
    const newlySeen = [...window._tm.all].filter(t => !window._tm.init.has(t));
    const result = newlySeen.length < 8 ? newlySeen : newlySeen.filter(t => !final.has(t));
    delete window._tm;
    return [...new Set(result)];
})();
"#;

async fn cdp_eval(ws_url: &str, expression: &str, await_promise: bool) -> Result<Value> {
    let (mut ws, _) = connect_async(ws_url).await?;
    let msg = json!({
        "id": 1,
        "method": "Runtime.evaluate",
        "params": {
            "expression": expression,
            "awaitPromise": await_promise,
            "returnByValue": true,
            "userGesture": true,
        }
    });
    ws.send(Message::Text(msg.to_string().into())).await?;
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
        let result = value
            .pointer("/result/result")
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(v) = result.get("value") {
            return Ok(v.clone());
        }
        if let Some(desc) = result.get("description").and_then(Value::as_str) {
            return Ok(Value::String(desc.to_string()));
        }
        return Ok(result);
    }
    bail!("CDP websocket closed before evaluation result")
}

async fn cdp_call(ws_url: &str, method: &str, params: Value) -> Result<Value> {
    let (mut ws, _) = connect_async(ws_url).await?;
    let msg = json!({
        "id": 1,
        "method": method,
        "params": params
    });
    ws.send(Message::Text(msg.to_string().into())).await?;
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
            bail!("CDP {method} error: {e}");
        }
        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
    }
    bail!("CDP websocket closed before command result")
}

fn simplify_browser_content(value: &Value, text_only: bool) -> String {
    let raw = value_to_string(value);
    let compact = if text_only {
        clean_text_output(&raw)
    } else {
        optimize_html_for_tokens(&raw)
    };
    smart_truncate_html(&compact, 35000)
}

fn clean_text_output(raw: &str) -> String {
    let mut out = String::new();
    let mut blank = 0usize;
    for line in raw.lines() {
        let cleaned = collapse_spaces(line).trim().to_string();
        if cleaned.is_empty() {
            blank += 1;
            if blank <= 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(&cleaned);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

fn collapse_spaces(s: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in s.chars() {
        if ch == ' ' || ch == '\t' || ch == '\r' {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out
}

fn optimize_html_for_tokens(raw: &str) -> String {
    let mut html = strip_html_block(raw, "script");
    html = strip_html_block(&html, "style");
    html = strip_html_block(&html, "noscript");
    html = collapse_svg(&html);
    rewrite_html_tags(&html)
}

fn strip_html_block(input: &str, tag: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::new();
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find(&open) {
        let start = pos + rel;
        out.push_str(&input[pos..start]);
        if let Some(end_rel) = lower[start..].find(&close) {
            pos = start + end_rel + close.len();
        } else {
            pos = input.len();
            break;
        }
    }
    out.push_str(&input[pos..]);
    out
}

fn collapse_svg(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::new();
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find("<svg") {
        let start = pos + rel;
        out.push_str(&input[pos..start]);
        if let Some(open_end_rel) = input[start..].find('>') {
            let open_end = start + open_end_rel;
            let self_closing = input[start..=open_end].trim_end().ends_with("/>");
            // Upstream simphtml.py calls `svg.clear(); svg.attrs = {}`: keep the tag,
            // but drop all attributes and child markup regardless of the original shape.
            out.push_str("<svg>");
            if self_closing {
                pos = open_end + 1;
                out.push_str("</svg>");
                continue;
            }
            out.push_str("</svg>");
            if let Some(close_rel) = lower[open_end + 1..].find("</svg>") {
                pos = open_end + 1 + close_rel + "</svg>".len();
            } else {
                pos = open_end + 1;
            }
        } else {
            pos = input.len();
            break;
        }
    }
    out.push_str(&input[pos..]);
    out
}

fn rewrite_html_tags(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end) = after.find('>') else {
            out.push_str(after);
            return out;
        };
        out.push_str(&optimize_html_tag(&after[..=end]));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn optimize_html_tag(tag: &str) -> String {
    if tag.starts_with("</")
        || tag.starts_with("<!--")
        || tag.starts_with("<!")
        || tag.starts_with("<?")
    {
        return tag.to_string();
    }
    let inner = tag.trim_start_matches('<').trim_end_matches('>').trim();
    let self_close = inner.ends_with('/');
    let inner = inner.trim_end_matches('/').trim();
    let mut split = inner.splitn(2, char::is_whitespace);
    let name = split.next().unwrap_or_default();
    if name.is_empty() {
        return tag.to_string();
    }
    let attrs = split.next().unwrap_or_default();
    let attrs = parse_attrs(attrs)
        .into_iter()
        .filter_map(|(k, v)| optimize_attr(&k, v.as_deref()))
        .collect::<Vec<_>>();
    let attrs = if attrs.is_empty() {
        String::new()
    } else {
        format!(" {}", attrs.join(" "))
    };
    format!("<{name}{attrs}{}>", if self_close { "/" } else { "" })
}

fn parse_attrs(mut s: &str) -> Vec<(String, Option<String>)> {
    let mut attrs = Vec::new();
    while !s.trim_start().is_empty() {
        s = s.trim_start();
        let key_len = s
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace() || *ch == '=' || *ch == '/' || *ch == '>')
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        if key_len == 0 {
            break;
        }
        let key = s[..key_len].to_string();
        s = &s[key_len..];
        s = s.trim_start();
        if !s.starts_with('=') {
            attrs.push((key, None));
            continue;
        }
        s = s[1..].trim_start();
        if let Some(q) = s.chars().next().filter(|c| *c == '"' || *c == '\'') {
            let body = &s[q.len_utf8()..];
            if let Some(end) = body.find(q) {
                attrs.push((key, Some(body[..end].to_string())));
                s = &body[end + q.len_utf8()..];
            } else {
                attrs.push((key, Some(body.to_string())));
                break;
            }
        } else {
            let end = s
                .char_indices()
                .find(|(_, ch)| ch.is_whitespace() || *ch == '>' || *ch == '/')
                .map(|(i, _)| i)
                .unwrap_or(s.len());
            attrs.push((key, Some(s[..end].to_string())));
            s = &s[end..];
        }
    }
    attrs
}

fn optimize_attr(key: &str, value: Option<&str>) -> Option<String> {
    const ALLOWED: &[&str] = &[
        "id",
        "class",
        "name",
        "src",
        "href",
        "alt",
        "value",
        "type",
        "placeholder",
        "disabled",
        "checked",
        "selected",
        "readonly",
        "required",
        "multiple",
        "role",
        "aria-label",
        "aria-expanded",
        "aria-hidden",
        "contenteditable",
        "title",
        "for",
        "action",
        "method",
        "target",
        "colspan",
        "rowspan",
    ];
    let key_l = key.to_ascii_lowercase();
    if key_l == "style" || key_l.starts_with("data-v") {
        return None;
    }
    let mut value = value.map(str::to_string);
    if !ALLOWED.contains(&key_l.as_str()) {
        if key_l.starts_with("data-") {
            if value.as_ref().is_some_and(|v| v.chars().count() > 20) {
                value = Some("__data__".into());
            }
        } else {
            return None;
        }
    }
    if let Some(v) = value.as_mut() {
        if key_l == "src" {
            if v.starts_with("data:") {
                *v = "__img__".into();
            } else if v.chars().count() > 30 {
                *v = "__url__".into();
            }
        } else if key_l == "href" && v.chars().count() > 30 {
            *v = "__link__".into();
        } else if key_l == "action" && v.chars().count() > 30 {
            *v = "__url__".into();
        } else if matches!(key_l.as_str(), "value" | "title" | "alt") && v.chars().count() > 100 {
            *v = format!("{} ...", v.chars().take(50).collect::<String>());
        }
        Some(format!(r#"{key}="{}""#, html_attr_escape(v)))
    } else {
        Some(key.to_string())
    }
}

fn html_attr_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn smart_truncate_html(html: &str, budget: usize) -> String {
    if html_char_len(html) <= budget {
        return html.to_string();
    }
    truncate_html_fragment(html, budget, 0)
}

#[derive(Debug, Clone)]
struct HtmlSegment {
    start: usize,
    end: usize,
    open_end: usize,
    close_start: usize,
    name: String,
    protected: bool,
}

#[derive(Debug, Clone)]
struct OpenTag {
    name: String,
    start: usize,
    open_end: usize,
}

fn truncate_html_fragment(html: &str, budget: usize, depth: usize) -> String {
    const CUT_THRESHOLD: usize = 8000;
    let total = html_char_len(html);
    if total <= budget {
        return html.to_string();
    }
    let children = top_level_html_segments(html);
    if children.is_empty() {
        return cut_html_leaf(html, budget);
    }
    let child_total: usize = children
        .iter()
        .map(|s| html_char_len(&html[s.start..s.end]))
        .sum();
    let self_len = total.saturating_sub(child_total);
    let child_budget = budget.saturating_sub(self_len);
    if child_budget == 0 {
        return cut_html_leaf(html, budget);
    }
    if children.len() == 1 && children[0].start == 0 && children[0].end == html.len() {
        let child = &children[0];
        if child.name.is_empty() {
            return cut_html_leaf(html, budget);
        }
        if child.open_end < child.close_start {
            let open = &html[child.start..child.open_end];
            let inner = &html[child.open_end..child.close_start];
            let close = &html[child.close_start..child.end];
            let overhead = html_char_len(open) + html_char_len(close);
            let inner_budget = budget.saturating_sub(overhead);
            if inner_budget > 0 {
                return format!(
                    "{open}{}{close}",
                    truncate_html_fragment(inner, inner_budget, depth + 1)
                );
            }
        }
        return cut_html_leaf(html, budget);
    }
    let over = child_total.saturating_sub(child_budget);
    if over == 0 {
        return html.to_string();
    }
    let mut indexed = children
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.protected)
        .map(|(i, s)| (i, html_char_len(&html[s.start..s.end])))
        .collect::<Vec<_>>();
    indexed.sort_by_key(|(_, len)| std::cmp::Reverse(*len));
    let mut targets = indexed.iter().take(3).copied().collect::<Vec<_>>();
    let target_total: usize = targets.iter().map(|(_, len)| *len).sum();
    let mut replacements = vec![None::<String>; children.len()];
    if target_total < over {
        let mut removed = 0usize;
        for (idx, seg) in children.iter().enumerate().rev() {
            if seg.protected {
                continue;
            }
            replacements[idx] = Some(String::new());
            removed += html_char_len(&html[seg.start..seg.end]);
            if removed >= over {
                break;
            }
        }
    } else {
        if let Some((_, max_size)) = targets.first().copied() {
            let filtered = targets
                .iter()
                .copied()
                .filter(|(_, len)| *len >= max_size / 10)
                .collect::<Vec<_>>();
            let filtered_total: usize = filtered.iter().map(|(_, len)| *len).sum();
            if filtered_total >= over {
                targets = filtered;
            }
        }
        let target_total: usize = targets.iter().map(|(_, len)| *len).sum();
        for (idx, len) in targets {
            let share = over.saturating_mul(len) / target_total.max(1);
            let keep = len.saturating_sub(share);
            let seg = &children[idx];
            let part = &html[seg.start..seg.end];
            replacements[idx] = Some(if keep == 0 {
                String::new()
            } else if keep > CUT_THRESHOLD && depth < 12 {
                truncate_html_fragment(part, keep, depth + 1)
            } else {
                cut_html_leaf_preserving_fake(part, keep)
            });
        }
    }
    let mut out = String::new();
    let mut pos = 0usize;
    for (idx, seg) in children.iter().enumerate() {
        out.push_str(&html[pos..seg.start]);
        if let Some(replacement) = &replacements[idx] {
            out.push_str(replacement);
        } else {
            out.push_str(&html[seg.start..seg.end]);
        }
        pos = seg.end;
    }
    out.push_str(&html[pos..]);
    out
}

fn cut_html_leaf_preserving_fake(html: &str, keep: usize) -> String {
    if !html.contains("[FAKE ELEMENT]") {
        return cut_html_leaf(html, keep);
    }
    let protected = top_level_html_segments(html)
        .into_iter()
        .filter(|s| s.protected)
        .map(|s| html[s.start..s.end].to_string())
        .collect::<Vec<_>>();
    let reserve: usize = protected.iter().map(|s| html_char_len(s)).sum();
    let base_keep = keep.saturating_sub(reserve);
    let mut out = cut_html_leaf(html, base_keep);
    for p in protected {
        if !out.contains(&p) {
            out.push_str(&p);
        }
    }
    out
}

fn cut_html_leaf(html: &str, keep: usize) -> String {
    let total = html_char_len(html);
    if total <= keep {
        return html.to_string();
    }
    let marker = format!(" [TRUNCATED {}k chars]", total.saturating_sub(keep) / 1000);
    let keep = keep.saturating_sub(html_char_len(&marker));
    let head = take_chars(html, keep);
    format!("{head}{marker}")
}

fn top_level_html_segments(input: &str) -> Vec<HtmlSegment> {
    let mut segments = Vec::new();
    let mut stack: Vec<OpenTag> = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = input[pos..].find('<') {
        let start = pos + rel;
        let Some(end_rel) = input[start..].find('>') else {
            break;
        };
        let tag_end = start + end_rel + 1;
        let raw = &input[start..tag_end];
        if raw.starts_with("<!--") || raw.starts_with("<!") || raw.starts_with("<?") {
            pos = tag_end;
            continue;
        }
        let (closing, name, self_closing) = parse_tag_token(raw);
        if name.is_empty() {
            pos = tag_end;
            continue;
        }
        if closing {
            if let Some(i) = stack.iter().rposition(|t| t.name == name) {
                let tag = stack.remove(i);
                stack.truncate(i);
                if stack.is_empty() {
                    let frag = &input[tag.start..tag_end];
                    segments.push(HtmlSegment {
                        start: tag.start,
                        end: tag_end,
                        open_end: tag.open_end,
                        close_start: start,
                        name,
                        protected: is_fake_hint_segment(frag),
                    });
                }
            }
        } else if self_closing || is_void_tag(&name) {
            if stack.is_empty() {
                let frag = &input[start..tag_end];
                segments.push(HtmlSegment {
                    start,
                    end: tag_end,
                    open_end: tag_end,
                    close_start: tag_end,
                    name,
                    protected: is_fake_hint_segment(frag),
                });
            }
        } else {
            stack.push(OpenTag {
                name,
                start,
                open_end: tag_end,
            });
        }
        pos = tag_end;
    }
    segments.sort_by_key(|s| s.start);
    segments
}

fn parse_tag_token(raw: &str) -> (bool, String, bool) {
    let inner = raw.trim_start_matches('<').trim_end_matches('>').trim();
    let closing = inner.starts_with('/');
    let inner = inner.trim_start_matches('/').trim();
    let self_closing = inner.ends_with('/');
    let name = inner
        .trim_end_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches('/')
        .to_ascii_lowercase();
    (closing, name, self_closing)
}

fn parse_tag_sig(inner: &str) -> (String, String) {
    let mut parts = inner.splitn(2, char::is_whitespace);
    let name = parts
        .next()
        .unwrap_or_default()
        .trim_matches('/')
        .to_ascii_lowercase();
    let attrs_raw = parts.next().unwrap_or_default();
    let attrs = normalize_attrs_without_track_id(attrs_raw);
    (name, attrs)
}

fn normalize_attrs_without_track_id(attrs: &str) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    while i < attrs.len() {
        while i < attrs.len() && attrs.as_bytes()[i].is_ascii_whitespace() {
            i += 1;
        }
        let key_start = i;
        while i < attrs.len()
            && !attrs.as_bytes()[i].is_ascii_whitespace()
            && attrs.as_bytes()[i] != b'='
        {
            i += 1;
        }
        let key = attrs[key_start..i].trim();
        while i < attrs.len() && attrs.as_bytes()[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < attrs.len() && attrs.as_bytes()[i] == b'=' {
            i += 1;
            while i < attrs.len() && attrs.as_bytes()[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < attrs.len() && matches!(attrs.as_bytes()[i], b'\'' | b'"') {
                let quote = attrs.as_bytes()[i];
                i += 1;
                let val_start = i;
                while i < attrs.len() && attrs.as_bytes()[i] != quote {
                    i += 1;
                }
                value = attrs[val_start..i.min(attrs.len())].to_string();
                if i < attrs.len() {
                    i += 1;
                }
            } else {
                let val_start = i;
                while i < attrs.len() && !attrs.as_bytes()[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = attrs[val_start..i].to_string();
            }
        }
        if !key.is_empty() && key != "data-track-id" {
            if !out.is_empty() {
                out.push(' ');
            }
            if value.is_empty() {
                out.push_str(key);
            } else {
                out.push_str(key);
                out.push('=');
                out.push_str(&value);
            }
        }
    }
    out
}

fn is_fake_hint_segment(fragment: &str) -> bool {
    fragment.contains("[FAKE ELEMENT]") && html_char_len(fragment) < 1000
}

fn is_void_tag(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn html_char_len(s: &str) -> usize {
    s.chars().count()
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[derive(Debug)]
struct HtmlSigNode {
    name: String,
    attrs: String,
    direct_text: String,
    raw: String,
    parent: Option<usize>,
}

#[derive(Debug)]
struct OpenHtmlSigNode {
    name: String,
    attrs: String,
    direct_text: String,
    start: usize,
    parent: Option<usize>,
}

impl HtmlSigNode {
    fn sig(&self) -> String {
        format!(
            "{}:{}:{}",
            self.name,
            self.attrs,
            collapse_spaces(&self.direct_text)
        )
    }
}

fn dom_diff_summary(before: &str, after: &str) -> String {
    let (changed, top_change) = find_changed_elements_like_upstream(before, after);
    let mut summary = format!("DOM变化量: {changed}");
    if let Some(top) = top_change {
        summary.push_str("\n最显著变化:\n");
        summary.push_str(&top);
    } else if changed == 0 {
        summary.push_str(" (页面无变化)");
    }
    summary
}

fn find_changed_elements_like_upstream(before: &str, after: &str) -> (usize, Option<String>) {
    let before_nodes = html_sig_nodes(before);
    let after_nodes = html_sig_nodes(after);
    let mut before_counts = BTreeMap::<String, usize>::new();
    let mut after_seen = BTreeMap::<String, usize>::new();
    for node in &before_nodes {
        *before_counts.entry(node.sig()).or_default() += 1;
    }
    let mut changed = Vec::<usize>::new();
    for (idx, node) in after_nodes.iter().enumerate() {
        let sig = node.sig();
        let seen = after_seen.entry(sig.clone()).or_default();
        *seen += 1;
        if *seen > before_counts.get(&sig).copied().unwrap_or(0) {
            changed.push(idx);
        }
    }
    if changed.is_empty() && before != after {
        for (idx, (before_node, after_node)) in before_nodes.iter().zip(&after_nodes).enumerate() {
            if before_node.sig() != after_node.sig() {
                changed.push(idx);
                break;
            }
        }
    }
    let changed_set = changed
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let boundaries = changed
        .iter()
        .copied()
        .filter(|idx| {
            after_nodes
                .get(*idx)
                .and_then(|n| n.parent)
                .is_none_or(|p| !changed_set.contains(&p))
        })
        .collect::<Vec<_>>();
    let mut top_candidates = boundaries;
    if top_candidates.is_empty() {
        top_candidates = changed.clone();
    }
    let top = top_candidates
        .into_iter()
        .filter_map(|idx| after_nodes.get(idx))
        .max_by_key(|node| node.raw.chars().count())
        .map(|node| {
            let raw = node.raw.clone();
            if raw.chars().count() <= 2000 {
                raw
            } else {
                format!("{}...[TRUNCATED]", take_chars(&raw, 2000))
            }
        });
    (changed.len(), top)
}

fn html_sig_nodes(html: &str) -> Vec<HtmlSigNode> {
    let mut nodes = Vec::<HtmlSigNode>::new();
    let mut stack = Vec::<OpenHtmlSigNode>::new();
    let mut pos = 0usize;
    while let Some(rel) = html[pos..].find('<') {
        let lt = pos + rel;
        append_direct_text(&mut stack, &html[pos..lt]);
        let Some(gt_rel) = html[lt..].find('>') else {
            break;
        };
        let gt = lt + gt_rel;
        let inside = html[lt + 1..gt].trim();
        if inside.starts_with('!') || inside.starts_with('?') {
            pos = gt + 1;
            continue;
        }
        if let Some(close_name) = inside.strip_prefix('/') {
            let close_name = close_name
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if let Some(open_idx) = stack.iter().rposition(|n| n.name == close_name) {
                while stack.len() > open_idx {
                    let open = stack.pop().expect("open html node");
                    let raw = html.get(open.start..gt + 1).unwrap_or_default().to_string();
                    nodes.push(HtmlSigNode {
                        name: open.name,
                        attrs: open.attrs,
                        direct_text: open.direct_text,
                        raw,
                        parent: open.parent,
                    });
                }
            }
        } else {
            let self_closing = inside.ends_with('/');
            let (name, attrs) = parse_tag_sig(inside.trim_end_matches('/').trim());
            if !name.is_empty() {
                let parent = stack.last().map(|_| nodes.len() + stack.len() - 1);
                if self_closing || is_void_tag(&name) {
                    nodes.push(HtmlSigNode {
                        name,
                        attrs,
                        direct_text: String::new(),
                        raw: html.get(lt..gt + 1).unwrap_or_default().to_string(),
                        parent,
                    });
                } else {
                    stack.push(OpenHtmlSigNode {
                        name,
                        attrs,
                        direct_text: String::new(),
                        start: lt,
                        parent,
                    });
                }
            }
        }
        pos = gt + 1;
    }
    append_direct_text(&mut stack, html.get(pos..).unwrap_or_default());
    while let Some(open) = stack.pop() {
        nodes.push(HtmlSigNode {
            name: open.name,
            attrs: open.attrs,
            direct_text: open.direct_text,
            raw: html.get(open.start..).unwrap_or_default().to_string(),
            parent: open.parent,
        });
    }
    nodes.reverse();
    nodes
}

fn append_direct_text(stack: &mut [OpenHtmlSigNode], text: &str) {
    if let Some(top) = stack.last_mut() {
        top.direct_text.push_str(text.trim());
    }
}

fn web_execute_suggestion(
    reloaded: bool,
    has_new_tabs: bool,
    diff: Option<&str>,
    transients: &[String],
) -> Option<String> {
    if reloaded || has_new_tabs {
        Some(
            if has_new_tabs {
                "页面已刷新，以上新标签页在执行期间连接。"
            } else {
                "页面已刷新，建议 web_scan 切换/确认当前页面。"
            }
            .into(),
        )
    } else if diff.is_some_and(|d| d.contains("页面无变化") || d.contains("页面无明显变化"))
        && transients.is_empty()
    {
        Some("页面无明显变化".into())
    } else {
        None
    }
}

fn log_memory_access(cfg: &AgentConfig, path: &Path) {
    if !path
        .to_string_lossy()
        .to_ascii_lowercase()
        .contains("memory")
    {
        return;
    }
    let stats_file = cfg.memory_dir.join("file_access_stats.json");
    let mut stats = fs::read_to_string(&stats_file)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Map<String, Value>>(&s).ok())
        .unwrap_or_default();
    let key = path
        .strip_prefix(&cfg.workspace_dir)
        .unwrap_or(path)
        .display()
        .to_string();
    let n = stats.get(&key).and_then(Value::as_u64).unwrap_or(0) + 1;
    stats.insert(key, json!(n));
    if let Some(parent) = stats_file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&Value::Object(stats)) {
        let _ = fs::write(stats_file, bytes);
    }
}

fn global_memory_prompt_for_tools(cfg: &AgentConfig) -> String {
    let suffix = if std::env::var("GA_LANG").unwrap_or_default() == "en" {
        "_en"
    } else {
        ""
    };
    let Ok(insight) = fs::read_to_string(cfg.memory_dir.join("global_mem_insight.txt")) else {
        return String::new();
    };
    let Ok(structure) = fs::read_to_string(
        cfg.resource_dir
            .join(format!("assets/insight_fixed_structure{suffix}.txt")),
    ) else {
        return String::new();
    };
    format!(
        "\ncwd = {} (./)\n\n[Memory] ({})\n{}\n{}/global_mem_insight.txt:\n{}\n",
        cfg.workspace_dir.display(),
        cfg.memory_dir.display(),
        structure,
        cfg.memory_dir.display(),
        insight
    )
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn expand_file_refs(text: &str, base: &Path) -> Result<String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("{{file:") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 7..];
        let Some(j) = after.find("}}") else {
            bail!("unterminated file ref")
        };
        let spec = &after[..j];
        let parts: Vec<_> = spec.rsplitn(3, ':').collect();
        if parts.len() != 3 {
            bail!("bad file ref: {spec}");
        }
        let end: usize = parts[0].parse()?;
        let start: usize = parts[1].parse()?;
        let path = base.join(parts[2]);
        let lines: Vec<_> = fs::read_to_string(path)?
            .lines()
            .map(str::to_string)
            .collect();
        out.push_str(&lines[start - 1..end].join("\n"));
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn extract_file_content(text: &str) -> Option<String> {
    let a = text.find("<file_content>")? + "<file_content>".len();
    let b = text[a..].find("</file_content>")? + a;
    Some(text[a..b].trim_matches('\n').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_agent_core::ToolDispatcher;
    use tempfile::tempdir;

    fn cfg(root: &Path) -> AgentConfig {
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
            openai_base_url: "http://x".into(),
            openai_api_key: "sk-test".into(),
            openai_model: "m".into(),
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

    #[tokio::test]
    async fn patch_requires_unique_match() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        fs::write(d.path().join("a.txt"), "x\nx\n").unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let r = t
            .dispatch(
                "file_patch",
                json!({"path":"a.txt","old_content":"x","new_content":"y"}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.data["status"], "error");
    }

    #[tokio::test]
    async fn file_patch_parity_errors_and_exact_success_shape() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        fs::write(d.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let response = AgentResponse {
            thinking: String::new(),
            content: String::new(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        let missing = t
            .dispatch(
                "file_patch",
                json!({"path":"a.txt","old_content":"delta","new_content":"x"}),
                &response,
                0,
            )
            .await
            .unwrap();
        assert_eq!(missing.data["status"], "error");
        assert!(missing.data["msg"].as_str().unwrap().contains("未找到匹配"));
        let empty = t
            .dispatch(
                "file_patch",
                json!({"path":"a.txt","old_content":"","new_content":"x"}),
                &response,
                0,
            )
            .await
            .unwrap();
        assert_eq!(empty.data["msg"], "old_content 为空，请确认 arguments");
        let ok = t
            .dispatch(
                "file_patch",
                json!({"path":"a.txt","old_content":"beta","new_content":"BETA"}),
                &response,
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            ok.data,
            json!({"status":"success","msg":"文件局部修改成功"})
        );
        assert_eq!(
            fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn file_read_returns_lines() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        fs::write(d.path().join("a.txt"), "a\nb\nc\n").unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let r = t
            .dispatch(
                "file_read",
                json!({"path":"a.txt","start":2,"count":1}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        assert!(r.data.as_str().unwrap().contains("2|b"));
        assert!(r.data.as_str().unwrap().contains("由于设置了show_linenos"));
    }

    #[tokio::test]
    async fn file_read_no_linenos_and_memory_sop_prompt_match_parity() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        fs::create_dir_all(d.path().join("memory")).unwrap();
        fs::write(d.path().join("memory/plan_sop.md"), "first\nsecond\n").unwrap();
        let cfg = cfg(d.path());
        let memory_path = cfg.memory_dir.join("plan_sop.md");
        let t = GenericToolDispatcher::new(cfg);
        let r = t
            .dispatch(
                "file_read",
                json!({"path":memory_path.display().to_string(),"show_linenos":false,"count":1}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        let s = r.data.as_str().unwrap();
        assert_eq!(s, "first");
        assert!(r.next_prompt.unwrap().contains("正在读取记忆或SOP文件"));
    }

    #[tokio::test]
    async fn file_read_keyword_falls_back_to_content() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        fs::write(d.path().join("a.txt"), "a\nb\nc\n").unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let r = t
            .dispatch(
                "file_read",
                json!({"path":"a.txt","keyword":"missing","start":2,"count":2}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        let s = r.data.as_str().unwrap();
        assert!(s.contains("Falling back"));
        assert!(s.contains("2|b"));
    }

    #[tokio::test]
    async fn file_write_supports_prepend_and_reports_bytes() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        fs::write(d.path().join("a.txt"), "tail").unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let r = t
            .dispatch(
                "file_write",
                json!({"path":"a.txt","mode":"prepend","content":"头"}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.data["status"], "success");
        assert_eq!(r.data["writed_bytes"], "头".len());
        assert_eq!(
            fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "头tail"
        );
    }

    #[tokio::test]
    async fn file_write_extracts_file_content_and_rejects_blank_like_upstream() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let response = AgentResponse {
            thinking: String::new(),
            content: "<file_content>\nhello\n</file_content>".into(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        let r = t
            .dispatch("file_write", json!({"path":"a.txt"}), &response, 0)
            .await
            .unwrap();
        assert_eq!(r.data["status"], "success");
        assert_eq!(fs::read_to_string(d.path().join("a.txt")).unwrap(), "hello");
        let blank = t
            .dispatch(
                "file_write",
                json!({"path":"blank.txt"}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        assert_eq!(blank.data["status"], "error");
        assert!(
            blank.data["msg"]
                .as_str()
                .unwrap()
                .contains("<file_content>")
        );
    }

    #[tokio::test]
    async fn file_write_root_path_is_workspace_root() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let r = t
            .dispatch(
                "file_write",
                json!({"path":"/matrix.html","content":"ok"}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.data["status"], "success");
        assert_eq!(
            fs::read_to_string(d.path().join("matrix.html")).unwrap(),
            "ok"
        );
    }

    #[tokio::test]
    async fn file_write_missing_path_defaults_to_index_at_workspace_root() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let r = t
            .dispatch(
                "file_write",
                json!({"content":"ok"}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.data["status"], "success");
        assert_eq!(
            fs::read_to_string(d.path().join("index.html")).unwrap(),
            "ok"
        );
    }

    #[tokio::test]
    async fn code_run_extracts_reply_code_block_and_inline_eval() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let response = AgentResponse {
            thinking: String::new(),
            content: "```python\n1 + 2\n```".into(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        let r = t
            .dispatch("code_run", json!({"inline_eval":true}), &response, 0)
            .await
            .unwrap();
        assert_eq!(r.data["status"], "success");
        assert!(r.data["stdout"].as_str().unwrap().contains('3'));
    }

    #[tokio::test]
    async fn code_run_inline_eval_exports_plan_mode_control() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let r = t
            .dispatch(
                "code_run",
                json!({"inline_eval":true,"script":"handler.enter_plan_mode(\"./plan_demo/plan.md\")"}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.data["status"], "success");
        assert_eq!(
            fs::read_to_string(d.path().join("temp/_plan_mode")).unwrap(),
            "./plan_demo/plan.md"
        );
    }

    #[tokio::test]
    async fn code_run_inline_eval_exports_done_hook() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        t.dispatch(
            "code_run",
            json!({"inline_eval":true,"script":"handler._done_hooks.append('重读自主任务sop')"}),
            &AgentResponse {
                thinking: String::new(),
                content: String::new(),
                tool_calls: vec![],
                raw: Value::Null,
            },
            0,
        )
        .await
        .unwrap();
        assert!(
            fs::read_to_string(d.path().join("temp/_done_hooks"))
                .unwrap()
                .contains("重读自主任务sop")
        );
    }

    #[test]
    fn code_run_prepends_header_for_python_file_mode() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("assets")).unwrap();
        fs::write(d.path().join("assets/code_run_header.py"), "# header").unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let out = t.with_code_run_header("print(1)");
        assert!(out.contains("KODA_AGENT_ROOT"));
        assert!(out.contains("KODA_MEMORY_DIR"));
        assert!(out.contains("# header\nprint(1)"));
    }

    #[tokio::test]
    async fn code_run_python_can_import_memory_vision_helpers_from_workspace_root() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("assets")).unwrap();
        fs::create_dir_all(d.path().join("memory")).unwrap();
        fs::write(d.path().join("assets/code_run_header.py"), "").unwrap();
        fs::write(
            d.path().join("memory/vision_api.py"),
            "def ask_vision(*a, **k): return 'vision-ok'\n",
        )
        .unwrap();
        let dispatcher = GenericToolDispatcher::new(cfg(d.path()));
        let response = AgentResponse {
            thinking: String::new(),
            content: String::new(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        let out = dispatcher
            .dispatch(
                "code_run",
                json!({"code":"from vision_api import ask_vision\nprint(ask_vision('x'))"}),
                &response,
                0,
            )
            .await
            .unwrap()
            .data;
        assert_eq!(out["status"], "success");
        assert!(
            out["stdout"]
                .as_str()
                .is_some_and(|s| s.contains("vision-ok"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn code_run_bash_long_output_is_smart_truncated() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let response = AgentResponse {
            thinking: String::new(),
            content: String::new(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        let r = t
            .dispatch(
                "code_run",
                json!({"type":"bash","code":"python3 - <<'PY'
print('HEAD')
print('x' * 24000)
print('TAIL')
PY"}),
                &response,
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.data["status"], "success");
        let stdout = r.data["stdout"].as_str().unwrap();
        assert!(stdout.contains("HEAD"));
        assert!(stdout.contains("TAIL"));
        assert!(stdout.contains("[omitted long output]"));
        assert!(stdout.chars().count() < 14000);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn code_run_bash_error_combines_stdout_stderr_and_exit_code() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let response = AgentResponse {
            thinking: String::new(),
            content: String::new(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        let r = t
            .dispatch(
                "code_run",
                json!({"type":"bash","code":"echo out; echo err >&2; exit 7"}),
                &response,
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.data["status"], "error");
        assert_eq!(r.data["exit_code"], 7);
        let stdout = r.data["stdout"].as_str().unwrap();
        assert!(stdout.contains("out"));
        assert!(stdout.contains("err"));
    }

    #[tokio::test]
    async fn code_run_honors_stop_signal_file() {
        let d = tempdir().unwrap();
        let stop_path = d.path().join("_stop_signal");
        #[cfg(windows)]
        let mut cmd = {
            let mut cmd = Command::new("powershell");
            cmd.arg("-NoProfile")
                .arg("-Command")
                .arg("Write-Output start; Start-Sleep -Seconds 5; Write-Output end");
            cmd
        };
        #[cfg(unix)]
        let mut cmd = Command::new("bash");
        #[cfg(unix)]
        cmd.arg("-c").arg("echo start; sleep 5; echo end");
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let signal = stop_path.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(300)).await;
            fs::write(signal, "1").unwrap();
        });
        let (_code, status, stdout) =
            run_command_collecting_output(cmd, Duration::from_secs(10), &stop_path)
                .await
                .unwrap();
        assert_eq!(status, "error");
        assert!(stdout.contains("start"));
        assert!(stdout.contains("[Stopped]"));
        assert!(!stdout.contains("end"));
    }

    #[tokio::test]
    async fn update_working_checkpoint_exports_runtime_injections() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let r = t
            .dispatch(
                "update_working_checkpoint",
                json!({"key_info":"重要事实","related_sop":"memory/plan_sop.md"}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.data["result"], "working key_info updated");
        assert_eq!(
            fs::read_to_string(d.path().join("temp/_keyinfo")).unwrap(),
            "重要事实"
        );
        assert!(
            fs::read_to_string(d.path().join("temp/_related_sop"))
                .unwrap()
                .contains("memory/plan_sop.md")
        );
    }

    #[tokio::test]
    async fn start_long_term_update_returns_l0_and_prompt() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("memory")).unwrap();
        fs::write(d.path().join("memory/memory_management_sop.md"), "L0 SOP").unwrap();
        fs::write(d.path().join("memory/global_mem_insight.txt"), "L1").unwrap();
        fs::create_dir_all(d.path().join("assets")).unwrap();
        fs::write(
            d.path().join("assets/insight_fixed_structure.txt"),
            "STRUCT",
        )
        .unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let r = t
            .dispatch(
                "start_long_term_update",
                json!({"reason":"learned"}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        assert!(r.data.as_str().unwrap().contains("This is L0"));
        assert!(r.next_prompt.unwrap().contains("总结提炼经验"));
        assert!(d.path().join("memory/long_term_updates.jsonl").exists());
    }

    #[test]
    fn web_scan_uses_simphtml_asset_when_available() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("assets")).unwrap();
        fs::write(
            d.path().join("assets/simphtml_opt.js"),
            "function optHTML(text_only){ return text_only ? 'text' : '<main></main>'; }",
        )
        .unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let expr = t.browser_extract_expr(true, false, "");
        assert!(expr.contains("function optHTML"));
        assert!(expr.ends_with("optHTML(true);"));
        fs::write(
            d.path().join("assets/simphtml_find_main_list.js"),
            "function findMainList(){ return []; }",
        )
        .unwrap();
        let expr = t.browser_extract_expr(false, true, "needle");
        assert!(expr.contains("function findMainList"));
        assert!(expr.contains("kodaCutlistHTML"));
        assert!(expr.contains("[FAKE ELEMENT]"));
    }

    #[test]
    fn optimize_html_removes_noisy_attrs_and_shortens_urls() {
        let html = r#"<main style="x" data-v-abc="1" data-long="abcdefghijklmnopqrstuvwxyz"><script>bad()</script><img src="data:image/png;base64,abc" alt="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"><a href="https://example.com/abcdefghijklmnopqrstuvwxyz">link</a><svg id="logo" class="icon" width="10"><path d="x"/></svg></main>"#;
        let out = optimize_html_for_tokens(html);
        assert!(!out.contains("<script>"));
        assert!(!out.contains("style="));
        assert!(!out.contains("data-v-abc"));
        assert!(out.contains(r#"data-long="__data__""#));
        assert!(out.contains(r#"src="__img__""#));
        assert!(out.contains(r#"href="__link__""#));
        assert!(out.contains("<svg></svg>"));
        assert!(out.contains("</svg>"));
        assert!(!out.contains("id=\"logo\""));
        assert!(!out.contains("class=\"icon\""));
        assert!(!out.contains("<path"));
    }

    #[test]
    fn text_cleanup_and_html_truncate_are_unicode_safe() {
        let cleaned = clean_text_output("  hello   world\n\n\n  下一行  文本  ");
        assert_eq!(cleaned, "hello world\n\n下一行 文本");
        let html = format!("<div>{}</div>", "开".repeat(40000));
        let truncated = smart_truncate_html(&html, 1000);
        assert!(truncated.contains("[TRUNCATED"));
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn smart_truncate_prefers_large_html_branches_and_preserves_fake_elements() {
        let huge_a = format!(
            "<section id=\"a\">{}</section>",
            "<p>alpha</p>".repeat(3000)
        );
        let huge_b = format!(
            "<section id=\"b\"><div>[FAKE ELEMENT] 200 more items hidden</div>{}</section>",
            "<article>beta</article>".repeat(3000)
        );
        let small = "<aside>keep me</aside>";
        let html = format!("<main>{huge_a}{small}{huge_b}</main>");
        let truncated = smart_truncate_html(&html, 5000);
        assert!(truncated.contains("[TRUNCATED"));
        assert!(truncated.contains("<main>"));
        assert!(truncated.contains("keep me"));
        assert!(truncated.contains("[FAKE ELEMENT] 200 more items hidden"));
        assert!(truncated.chars().count() < html.chars().count() / 3);
    }

    #[test]
    fn smart_truncate_tail_cuts_when_top_children_cannot_absorb_overflow() {
        let html = format!(
            "<main>{}</main>",
            (0..12)
                .map(|i| format!("<section id=\"s{i}\">{}</section>", "x".repeat(900)))
                .collect::<Vec<_>>()
                .join("")
        );
        let truncated = smart_truncate_html(&html, 2200);
        assert!(truncated.contains("id=\"s0\""));
        assert!(truncated.contains("id=\"s1\""));
        assert!(!truncated.contains("id=\"s11\""));
        assert!(truncated.chars().count() < html.chars().count());
    }

    #[test]
    fn top_level_html_segments_tracks_direct_children() {
        let html = "<main><section><p>a</p></section><aside>b</aside><img src=\"x\"></main>";
        let segments = top_level_html_segments(html);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].name, "main");
        let inner = &html[segments[0].open_end..segments[0].close_start];
        let kids = top_level_html_segments(inner);
        assert_eq!(
            kids.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["section", "aside", "img"]
        );
    }

    #[test]
    fn dom_diff_summary_reports_changed_region() {
        let diff = dom_diff_summary("<main>\na\n</main>", "<main>\nb\n</main>");
        assert!(diff.contains("DOM变化量"));
        assert!(diff.contains("最显著变化:"));
        assert!(diff.contains("<main>"));
        assert!(diff.contains('b'));
        let spa = dom_diff_summary(
            "<main><section id=\"app\"><h1>Home</h1></section></main>",
            "<main><section id=\"app\"><h1>Home</h1><article data-route=\"done\"><h2>Async route loaded</h2><p>second async chunk</p></article></section></main>",
        );
        assert!(spa.contains("DOM变化量"));
        assert!(spa.contains("最显著变化:"));
        assert!(spa.contains("second async chunk"));
        assert_eq!(
            dom_diff_summary("<main>a</main>", "<main>a</main>"),
            "DOM变化量: 0 (页面无变化)"
        );
    }

    #[test]
    fn tab_diff_and_suggestion_helpers_match_rich_js_monitoring() {
        let before = vec![
            json!({"id":"a","url":"https://old.example","title":"old"}),
            json!({"id":"b","url":"https://stay.example","title":"stay"}),
        ];
        let after = vec![
            json!({"id":"b","url":"https://stay.example","title":"stay"}),
            json!({"id":"c","url":"https://new.example","title":"new"}),
        ];
        let ids = tab_ids(&before);
        let new_tabs = new_tabs_since(&ids, &after);
        assert_eq!(new_tabs.len(), 1);
        assert_eq!(new_tabs[0]["id"], "c");
        assert!(
            web_execute_suggestion(false, true, None, &[])
                .unwrap()
                .contains("以上新标签页在执行期间连接")
        );
        assert_eq!(
            web_execute_suggestion(true, false, None, &[]).as_deref(),
            Some("页面已刷新，建议 web_scan 切换/确认当前页面。")
        );
        assert_eq!(
            web_execute_suggestion(false, false, Some("页面无明显变化"), &[]).as_deref(),
            Some("页面无明显变化")
        );
        assert!(TEMP_MONITOR_JS.contains("startStrMonitor"));
        assert!(TEMP_MONITOR_STOP_JS.contains("stopStrMonitor"));
    }

    #[test]
    fn tmwd_bridge_json_commands_are_detected_before_plain_js_eval() {
        let cmd = parse_tmwd_bridge_command(r#"{"cmd":"tabs"}"#).unwrap();
        assert_eq!(cmd["cmd"], "tabs");
        let cmd =
            parse_tmwd_bridge_command(r#"{"cmd":"cdp","method":"Runtime.evaluate"}"#).unwrap();
        assert_eq!(cmd["method"], "Runtime.evaluate");
        assert!(parse_tmwd_bridge_command("({cmd:'tabs'})").is_none());
        assert!(parse_tmwd_bridge_command(r#"{"hello":"world"}"#).is_none());
    }

    #[test]
    fn tmwd_batch_refs_resolve_previous_results() {
        let results = vec![json!({"ok":true,"data":[{"id":123,"url":"https://example.com"}]})];
        let cmd = json!({
            "cmd":"cdp",
            "tabId":"$0.data.0.id",
            "params":{"url":"$0.data.0.url","nested":["$0.data.0.id"]}
        });
        let resolved = resolve_batch_refs(cmd, &results);
        assert_eq!(resolved["tabId"], 123);
        assert_eq!(resolved["params"]["url"], "https://example.com");
        assert_eq!(resolved["params"]["nested"][0], 123);
        assert_eq!(
            resolve_batch_refs(json!("$9.nope"), &results),
            json!("$9.nope")
        );
    }

    #[tokio::test]
    async fn tmwd_batch_reports_extension_only_and_nested_batch_errors_without_panic() {
        let cmd = json!({
            "cmd":"batch",
            "commands":[
                {"cmd":"management","method":"list"},
                {"cmd":"contentSettings","type":"automaticDownloads","setting":"allow"},
                {"cmd":"batch","commands":[]},
                {"cmd":"unknown"}
            ]
        });
        let out = execute_bridge_batch(&cmd).await.unwrap();
        assert_eq!(out["ok"], true);
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 4);
        assert!(
            results[0]["error"]
                .as_str()
                .unwrap()
                .contains("installed tmwd_cdp_bridge")
        );
        assert!(
            results[1]["error"]
                .as_str()
                .unwrap()
                .contains("extension APIs")
        );
        assert!(
            results[2]["error"]
                .as_str()
                .unwrap()
                .contains("nested tmwd batch")
        );
        assert!(
            results[3]["error"]
                .as_str()
                .unwrap()
                .contains("unknown tmwd bridge cmd")
        );
    }

    #[test]
    fn tmwd_master_error_message_prefers_error_field_text() {
        assert_eq!(
            tmwd_error_message(&json!({"error":{"message":"Cannot read properties of null"}})),
            "Cannot read properties of null"
        );
        assert_eq!(tmwd_error_message(&json!({"error":"plain"})), "plain");
    }

    #[test]
    fn web_execute_js_extracts_reply_block() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let response = AgentResponse {
            thinking: String::new(),
            content: "```javascript\nreturn document.title\n```".into(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        assert_eq!(
            t.resolve_js_script(&json!({}), &response).unwrap(),
            "return document.title"
        );
    }

    #[test]
    fn web_execute_js_reads_script_path() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("temp")).unwrap();
        fs::write(d.path().join("script.js"), "return 42").unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let response = AgentResponse {
            thinking: String::new(),
            content: String::new(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        assert_eq!(
            t.resolve_js_script(&json!({"script":"script.js"}), &response)
                .unwrap(),
            "return 42"
        );
    }

    #[test]
    fn code_run_powershell_uses_pwsh_family_not_bash_alias() {
        assert_eq!(
            extract_code_block("```pwsh\nWrite-Output 42\n```", "powershell").unwrap(),
            "Write-Output 42"
        );
        assert!(find_program(&["definitely-not-a-koda-binary"]).is_none());
    }

    #[tokio::test]
    async fn file_read_not_found_suggests_similar_file() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("src")).unwrap();
        fs::write(d.path().join("src/matrix_intro.html"), "ok").unwrap();
        let t = GenericToolDispatcher::new(cfg(d.path()));
        let r = t
            .dispatch(
                "file_read",
                json!({"path":"matrix_intr.html"}),
                &AgentResponse {
                    thinking: String::new(),
                    content: String::new(),
                    tool_calls: vec![],
                    raw: Value::Null,
                },
                0,
            )
            .await
            .unwrap();
        let s = r.data.as_str().unwrap();
        assert!(s.contains("Did you mean"));
        assert!(s.contains("matrix_intro.html"));
    }

    #[test]
    fn truncate_line_is_unicode_safe() {
        let line = "开".repeat(8001);
        let truncated = truncate_line(&line);
        assert!(truncated.ends_with(" ... [TRUNCATED]"));
        assert_eq!(truncated.chars().filter(|c| *c == '开').count(), 8000);
    }

    #[test]
    fn filename_similarity_finds_close_names() {
        assert!(filename_similarity("matrix_intr.html", "matrix_intro.html") > 0.8);
    }
}
