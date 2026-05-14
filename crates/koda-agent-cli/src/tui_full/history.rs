use std::{fs, path::Path};

use koda_agent_core::{AgentConfig, ChatMessage};
use serde::Deserialize;

use super::render::trim_chars;
use super::state::{
    SessionStatus, StreamMetrics, StreamState, ThinkingState, TimelineItem, TuiSessionState,
    UsageStats,
};

const MAX_HISTORY_SESSIONS: usize = 8;
const MAX_HISTORY_LINES: usize = 60;

#[derive(Debug, Clone)]
pub(super) struct LoadedHistorySession {
    pub(super) session: TuiSessionState,
    pub(super) messages: Vec<ChatMessage>,
    pub(super) history_info: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawSessionFile {
    session: Option<String>,
    created_at: Option<String>,
    #[serde(default)]
    history: Vec<String>,
    #[serde(default)]
    messages: Vec<ChatMessage>,
}

pub(super) fn load_recent_history_sessions(
    cfg: &AgentConfig,
    start_id: usize,
) -> Vec<LoadedHistorySession> {
    let dir = cfg.memory_dir.join("L4_raw_sessions");
    let mut paths = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_session_json(path))
            .collect::<Vec<_>>(),
        Err(_) => return Vec::new(),
    };
    paths.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    // 按session ID去重，保留最新的（文件名排序后第一个）
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter_map(|path| load_history_session_file(&path).ok())
        .filter(|raw| !raw.history.is_empty() || !raw.messages.is_empty())
        .filter(|raw| {
            let key = raw.session.clone().unwrap_or_default();
            seen.insert(key) // insert返回true表示首次出现
        })
        .take(MAX_HISTORY_SESSIONS)
        .enumerate()
        .map(|(idx, raw)| history_session_to_tui(raw, start_id + idx))
        .collect()
}

fn is_session_json(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("json")
        && path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name.starts_with("session_"))
}

fn load_history_session_file(path: &Path) -> serde_json::Result<RawSessionFile> {
    let bytes = fs::read(path).map_err(serde_json::Error::io)?;
    serde_json::from_slice(&bytes)
}

fn history_session_to_tui(raw: RawSessionFile, id: usize) -> LoadedHistorySession {
    let title =
        history_prompt_title(&raw.history, raw.created_at.as_deref()).unwrap_or_else(|| {
            raw.session
                .as_deref()
                .map(history_session_title)
                .unwrap_or_else(|| format!("history-{id}"))
        });
    let history_info = raw.history.clone();
    let mut timeline = Vec::new();
    let created = raw.created_at.as_deref().unwrap_or("unknown time");
    timeline.push(TimelineItem::System(format!(
        "Loaded historical session from L4 memory ({created}). Submit a new prompt to continue from its saved messages."
    )));
    // 优先使用messages结构化数据，fallback到history文本行
    if !raw.messages.is_empty() && raw.messages.iter().any(|m| m.role != "system") {
        let items = messages_to_timeline(&raw.messages);
        timeline.extend(items.into_iter().filter(is_visible_timeline));
    } else {
        timeline.extend(history_lines_to_timeline(
            &raw.history
                .iter()
                .rev()
                .take(MAX_HISTORY_LINES)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>(),
        ));
    }

    LoadedHistorySession {
        session: TuiSessionState {
            id,
            name: title,
            status: SessionStatus::Idle,
            timeline,
            fold: true,
            last_error: None,
            active_turn: None,
            last_tool: None,
            pending_ask: None,
            unread_events: 0,
            completed_tasks: 0,
            failed_tasks: 0,
            last_notice: Some("loaded from L4 memory".into()),
            timeline_scroll: 0,
            timeline_follow_tail: true,
            timeline_unseen: 0,
            timeline_revision: 0,
            timeline_cache: None,
            usage: UsageStats::default(),
            stream_state: StreamState::Idle,
            thinking_state: ThinkingState::Unavailable,
            stream_metrics: StreamMetrics::default(),
            session_started_at: None,
            turn_started_at: None,
            last_turn_elapsed: None,
        },
        messages: raw.messages,
        history_info,
    }
}

fn history_session_title(session: &str) -> String {
    let short = session.strip_prefix("session_").unwrap_or(session);
    trim_chars(&format!("hist-{short}"), 32)
}

fn history_prompt_title(history: &[String], created_at: Option<&str>) -> Option<String> {
    let time_prefix = created_at
        .and_then(|s| s.get(5..16)) // "2026-05-11T12:04" → "05-11T12:04"
        .map(|s| s.replace('T', " "))
        .unwrap_or_default();
    history.iter().find_map(|line| {
        let prompt = line.trim().strip_prefix("[USER]:")?.trim();
        if prompt.is_empty() {
            None
        } else {
            let title = compact_title(prompt);
            if time_prefix.is_empty() {
                Some(trim_chars(&format!("hist {title}"), 32))
            } else {
                Some(trim_chars(&format!("{time_prefix} {title}"), 32))
            }
        }
    })
}

fn compact_title(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    compact
        .trim_matches(|ch: char| ch == '"' || ch == '\'' || ch.is_ascii_punctuation())
        .to_string()
}

/// 识别系统内部注入的消息，不展示在用户时间线中
fn is_system_infra_line(line: &str) -> bool {
    line.starts_with("[SYSTEM")
        || line.starts_with("[Resource Memory]")
        || line.starts_with("<earlier_context>")
        || line.starts_with("<history>")
        || line.starts_with("[MASTER]")
        || line.starts_with("[WORKING MEMORY]")
        || line.starts_with("[Peer]")
}

/// 跳过系统注入的用户消息（[SYSTEM, [DANGER, <earlier_context>, <history>, cwd=等）
fn is_visible_timeline(item: &TimelineItem) -> bool {
    match item {
        TimelineItem::User(text) => !is_system_injected(text),
        _ => true,
    }
}

/// 从history行列表生成timeline，过滤系统infra消息
fn history_lines_to_timeline(lines: &[String]) -> Vec<TimelineItem> {
    lines
        .iter()
        .map(|l| history_line_to_timeline(l))
        .filter(is_visible_timeline)
        .collect()
}

fn history_line_to_timeline(line: &str) -> TimelineItem {
    let trimmed = line.trim();
    // 过滤系统内部消息，不展示在用户时间线中
    if is_system_infra_line(trimmed) {
        return TimelineItem::System(String::new()); // 空字符串，渲染时可跳过
    }
    if let Some(rest) = trimmed.strip_prefix("[USER]:") {
        TimelineItem::User(rest.trim().to_string())
    } else if let Some(rest) = trimmed.strip_prefix("[Agent]") {
        parse_agent_history_line(rest)
    } else if let Some(rest) = trimmed.strip_prefix("[ASSISTANT]:") {
        TimelineItem::Assistant(rest.trim().to_string())
    } else if let Some(rest) = trimmed.strip_prefix("[ToolCall]") {
        parse_tool_call_history_line(rest.trim())
            .unwrap_or_else(|| TimelineItem::System(trimmed.to_string()))
    } else if let Some(rest) = trimmed.strip_prefix("[ToolResult]") {
        parse_tool_result_history_line(rest.trim())
    } else {
        TimelineItem::System(trimmed.to_string())
    }
}

fn parse_agent_history_line(rest: &str) -> TimelineItem {
    let trimmed = rest.trim_start_matches(':').trim();
    if let Some(item) = parse_tool_call_summary(trimmed) {
        return item;
    }
    // Detect tool result lines from line-mode TUI history:
    //   "调用工具结果 {name} {data}"
    if let Some(item) = parse_named_payload(
        trimmed
            .strip_prefix("调用工具结果 ")
            .or_else(|| trimmed.strip_prefix("tool result ")),
        |name, data| TimelineItem::ToolResult {
            name,
            args: String::new(),
            data,
        },
    ) {
        return item;
    }
    TimelineItem::Assistant(trimmed.to_string())
}

fn parse_tool_call_summary(trimmed: &str) -> Option<TimelineItem> {
    // Current fallback_turn_summary format is Chinese. Keep English and
    // structured variants here so historical rendering is not tied to one UI
    // locale forever.
    for prefix in ["调用工具", "tool call ", "called tool "] {
        if let Some(tool_part) = trimmed.strip_prefix(prefix)
            && let Some((name, args)) = tool_part.split_once(", args: ")
        {
            let name = name.trim();
            if is_tool_name_like(name) {
                return Some(TimelineItem::ToolCall {
                    name: name.to_string(),
                    args: args.trim().to_string(),
                });
            }
        }
    }
    None
}

fn parse_tool_call_history_line(rest: &str) -> Option<TimelineItem> {
    let rest = rest.trim_start_matches(':').trim();
    if let Some(item) = parse_tool_call_json(rest) {
        return Some(item);
    }
    parse_named_payload(Some(rest), |name, args| TimelineItem::ToolCall {
        name,
        args,
    })
}

fn parse_tool_call_json(rest: &str) -> Option<TimelineItem> {
    let value = serde_json::from_str::<serde_json::Value>(rest).ok()?;
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)?
        .trim();
    if !is_tool_name_like(name) {
        return None;
    }
    let args = value
        .get("args")
        .or_else(|| value.get("arguments"))
        .map(|v| {
            v.as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default();
    Some(TimelineItem::ToolCall {
        name: name.to_string(),
        args,
    })
}

fn parse_tool_result_history_line(rest: &str) -> TimelineItem {
    // Format: "[ToolResult] {name} {data}"
    let rest = rest.trim_start_matches(':').trim();
    parse_named_payload(Some(rest), |name, data| TimelineItem::ToolResult {
        name,
        args: String::new(),
        data,
    })
    .unwrap_or_else(|| TimelineItem::System(format!("[ToolResult] {rest}")))
}

fn parse_named_payload<F>(rest: Option<&str>, build: F) -> Option<TimelineItem>
where
    F: FnOnce(String, String) -> TimelineItem,
{
    let rest = rest?.trim();
    let (name, payload) = rest.split_once(' ')?;
    let name = name.trim();
    if !is_tool_name_like(name) {
        return None;
    }
    Some(build(name.to_string(), payload.trim().to_string()))
}

fn is_tool_name_like(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
}

/// 提取<summary>...</summary>中的内容
fn extract_summary(text: &str) -> Option<String> {
    let start_tag = "<summary>";
    let end_tag = "</summary>";
    let start = text.find(start_tag)?;
    let content_start = start + start_tag.len();
    let end = text[content_start..].find(end_tag)? + content_start;
    let content = text[content_start..end].trim();
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

/// 剥离<summary>...</summary>后的正文
fn strip_summary(text: &str) -> String {
    let start_tag = "<summary>";
    let end_tag = "</summary>";
    let start = match text.find(start_tag) {
        Some(s) => s,
        None => return text.to_string(),
    };
    let content_start = start + start_tag.len();
    let end = match text[content_start..].find(end_tag) {
        Some(e) => e + content_start + end_tag.len(),
        None => return text.to_string(),
    };
    let mut result = String::new();
    if start > 0 {
        result.push_str(&text[..start]);
    }
    let rest = text[end..].trim_start();
    if !rest.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(rest);
    }
    result
}

/// 判断user消息内容是否为系统注入（非真实用户输入）
/// trim后匹配已知系统注入前缀
fn is_system_injected(text: &str) -> bool {
    let t = text.trim();
    t.starts_with("[SYSTEM")
        || t.starts_with("[DANGER")
        || t.starts_with("<earlier_context>")
        || t.starts_with("<history>")
        || t.starts_with("<system-recall>")
        || t.starts_with("### [WORKING MEMORY")
        || t.starts_with("cwd = ")
}

// ============================================================================
// messages_to_timeline: 从结构化ChatMessage渲染TimelineItem
// ============================================================================

fn messages_to_timeline(messages: &[ChatMessage]) -> Vec<TimelineItem> {
    let mut result = Vec::new();
    for msg in messages {
        match msg.role.as_str() {
            "system" => {} // 跳过system消息
            "user" => {
                // 过滤含tool_results的消息（工具结果回显，非用户输入）
                if let Some(obj) = msg.content.as_object()
                    && obj.contains_key("tool_results")
                {
                    continue;
                }
                if let Some(text) = extract_user_text(&msg.content) {
                    if is_system_injected(&text) {
                        continue;
                    }
                    result.push(TimelineItem::User(text));
                }
            }
            "assistant" => {
                // assistant的content可能是纯文本或JSON对象
                if let Some(obj) = msg.content.as_object() {
                    // 提取thinking字段
                    if let Some(thinking) = obj.get("thinking").and_then(|v| v.as_str()) {
                        let trimmed = thinking.trim();
                        if !trimmed.is_empty() {
                            result.push(TimelineItem::Thinking(trimmed.to_string()));
                        }
                    }
                    // 提取text字段
                    if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                        let text = text.trim();
                        if !text.is_empty() {
                            // 提取<summary>...</summary>
                            if let Some(summary) = extract_summary(text) {
                                result.push(TimelineItem::Summary(summary));
                            }
                            // 剥离<summary>后的正文
                            let body = strip_summary(text);
                            if !body.is_empty() {
                                result.push(TimelineItem::Assistant(body));
                            }
                        }
                    }
                    // 提取tool_calls
                    if let Some(calls) = obj.get("tool_calls").and_then(|v| v.as_array()) {
                        for call in calls {
                            if let Some(item) = parse_tool_call_value(call) {
                                result.push(item);
                            }
                        }
                    }
                } else if let Some(text) = msg.content.as_str() {
                    // 纯文本assistant消息
                    let text = text.trim();
                    if !text.is_empty() {
                        if let Some(summary) = extract_summary(text) {
                            result.push(TimelineItem::Summary(summary));
                        }
                        let body = strip_summary(text);
                        if !body.is_empty() {
                            result.push(TimelineItem::Assistant(body));
                        }
                    }
                }
            }
            "tool" => {
                // tool消息通常是工具执行结果
                if let Some(obj) = msg.content.as_object() {
                    let name = obj
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let content = obj
                        .get("content")
                        .map(|v| {
                            v.as_str()
                                .map(ToOwned::to_owned)
                                .unwrap_or_else(|| v.to_string())
                        })
                        .unwrap_or_default();
                    result.push(TimelineItem::ToolResult {
                        name: name.to_string(),
                        args: String::new(),
                        data: content,
                    });
                }
            }
            _ => {} // 忽略其他role
        }
    }
    result
}

/// 从user消息的content中提取文本
/// content可能是：
///   - 纯文本字符串
///   - JSON对象 {content: "...", ...}（含系统提示时）
fn extract_user_text(content: &serde_json::Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        // 纯文本，但可能包含系统提示前缀
        // 简单处理：取最后一段用户消息
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        // 如果包含"[USER]:"或类似的分隔符，取最后一部分
        // 否则直接返回
        Some(trimmed.to_string())
    } else if let Some(obj) = content.as_object() {
        // JSON对象，尝试提取content字段
        obj.get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    }
}

/// 解析tool_call的JSON值为TimelineItem
fn parse_tool_call_value(value: &serde_json::Value) -> Option<TimelineItem> {
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)?
        .trim();
    if !is_tool_name_like(name) {
        return None;
    }
    let args = value
        .get("args")
        .or_else(|| value.get("arguments"))
        .map(|v| {
            v.as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default();
    Some(TimelineItem::ToolCall {
        name: name.to_string(),
        args,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_agent_core::{AgentConfig, MixinConfig};
    use serde_json::json;
    use std::{collections::BTreeMap, fs};
    use tempfile::TempDir;

    fn cfg_with_memory(dir: &TempDir) -> AgentConfig {
        AgentConfig {
            home_dir: dir.path().to_path_buf(),
            workspace_dir: dir.path().to_path_buf(),
            resource_dir: dir.path().to_path_buf(),
            root_dir: dir.path().to_path_buf(),
            temp_dir: dir.path().join("temp"),
            memory_dir: dir.path().join("memory"),
            logs_dir: dir.path().join("logs"),
            sessions_dir: dir.path().join("sessions"),
            browser_dir: dir.path().join("browser"),
            openai_base_url: "http://localhost".into(),
            openai_api_key: "test".into(),
            openai_model: "mock".into(),
            llm_api_style: "chat".into(),
            auth_scheme: None,
            auth_header: None,
            max_turns: 3,
            verbose: false,
            stream: false,
            timeout_secs: 30,
            connect_timeout_secs: 5,
            verify_tls: true,
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            thinking_type: None,
            thinking_budget_tokens: None,
            service_tier: None,
            proxy: None,
            failover: false,
            custom_headers: BTreeMap::new(),
            mixin: MixinConfig::default(),
            llm_configs: Vec::new(),
        }
    }

    #[test]
    fn loads_recent_l4_sessions_as_tui_preview_sessions() {
        let dir = TempDir::new().unwrap();
        let cfg = cfg_with_memory(&dir);
        let l4 = cfg.memory_dir.join("L4_raw_sessions");
        fs::create_dir_all(&l4).unwrap();
        fs::write(
            l4.join("session_20260510_120000_1.json"),
            serde_json::to_vec_pretty(&json!({
                "session": "session_20260510_120000_1",
                "created_at": "2026-05-10T12:00:00+08:00",
                "history": ["[USER]: 你好", "[Agent] 调用工具file_read, args: {}", "[Agent] 完成"]
            }))
            .unwrap(),
        )
        .unwrap();

        let loaded = load_recent_history_sessions(&cfg, 2);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].session.id, 2);
        assert_eq!(loaded[0].session.name, "05-10 12:00 你好");
        assert_eq!(loaded[0].history_info.len(), 3);
        assert_eq!(loaded[0].messages.len(), 0);

        // timeline[0] = System("Loaded historical session...")
        assert!(matches!(
            loaded[0].session.timeline[1],
            TimelineItem::User(_)
        ));
        // tool call line parsed as ToolCall, not Assistant
        assert!(
            matches!(&loaded[0].session.timeline[2], TimelineItem::ToolCall { name, args } if name == "file_read" && args == "{}"),
            "expected ToolCall(file_read), got {:?}",
            loaded[0].session.timeline[2]
        );
        assert!(matches!(
            loaded[0].session.timeline[3],
            TimelineItem::Assistant(_)
        ));
    }

    #[test]
    fn parses_tool_call_and_tool_result_from_history_lines() {
        assert!(matches!(
            history_line_to_timeline("[Agent] 调用工具web_scan, args: {\"url\":\"https://example.test\"}"),
            TimelineItem::ToolCall { name, args } if name == "web_scan" && args.contains("example.test")
        ));
        assert!(matches!(
            history_line_to_timeline("[Agent] 直接回答了用户问题"),
            TimelineItem::Assistant(text) if text == "直接回答了用户问题"
        ));
        assert!(matches!(
            history_line_to_timeline("[ToolResult] file_read ok"),
            TimelineItem::ToolResult { name, data, .. } if name == "file_read" && data == "ok"
        ));
        assert!(matches!(
            history_line_to_timeline("[ToolCall] file_patch {\"path\":\"README.md\"}"),
            TimelineItem::ToolCall { name, args } if name == "file_patch" && args.contains("README.md")
        ));
        assert!(matches!(
            history_line_to_timeline("[ToolCall] {\"name\":\"ask_user\",\"arguments\":{\"question\":\"确认?\"}}"),
            TimelineItem::ToolCall { name, args } if name == "ask_user" && args.contains("question")
        ));
        assert!(matches!(
            history_line_to_timeline("[Agent] tool call code_run, args: {\"code\":\"1+1\"}"),
            TimelineItem::ToolCall { name, args } if name == "code_run" && args.contains("1+1")
        ));
    }
}
