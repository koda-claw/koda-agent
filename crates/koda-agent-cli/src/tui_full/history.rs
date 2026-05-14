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

    paths
        .into_iter()
        .filter_map(|path| load_history_session_file(&path).ok())
        .filter(|raw| !raw.history.is_empty() || !raw.messages.is_empty())
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
    let title = history_prompt_title(&raw.history).unwrap_or_else(|| {
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
    timeline.extend(
        raw.history
            .iter()
            .rev()
            .take(MAX_HISTORY_LINES)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|line| history_line_to_timeline(line)),
    );

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

fn history_prompt_title(history: &[String]) -> Option<String> {
    history.iter().find_map(|line| {
        let prompt = line.trim().strip_prefix("[USER]:")?.trim();
        if prompt.is_empty() {
            None
        } else {
            Some(trim_chars(&format!("hist {}", compact_title(prompt)), 32))
        }
    })
}

fn compact_title(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    compact
        .trim_matches(|ch: char| ch == '"' || ch == '\'' || ch.is_ascii_punctuation())
        .to_string()
}

fn history_line_to_timeline(line: &str) -> TimelineItem {
    let trimmed = line.trim();
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
                "history": ["[USER]: 你好", "[Agent] 调用工具file_read, args: {}", "[Agent] 完成"],
                "messages": [{"role":"user", "content":"你好"}]
            }))
            .unwrap(),
        )
        .unwrap();

        let loaded = load_recent_history_sessions(&cfg, 2);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].session.id, 2);
        assert_eq!(loaded[0].session.name, "hist 你好");
        assert_eq!(loaded[0].history_info.len(), 3);
        assert_eq!(loaded[0].messages.len(), 1);

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
