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
    let title = raw
        .session
        .as_deref()
        .map(history_session_title)
        .unwrap_or_else(|| format!("history-{id}"));
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
        },
        messages: raw.messages,
        history_info,
    }
}

fn history_session_title(session: &str) -> String {
    let short = session.strip_prefix("session_").unwrap_or(session);
    trim_chars(&format!("hist-{short}"), 32)
}

fn history_line_to_timeline(line: &str) -> TimelineItem {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("[USER]:") {
        TimelineItem::User(rest.trim().to_string())
    } else if let Some(rest) = trimmed.strip_prefix("[Agent]") {
        TimelineItem::Assistant(rest.trim_start_matches(':').trim().to_string())
    } else if let Some(rest) = trimmed.strip_prefix("[ASSISTANT]:") {
        TimelineItem::Assistant(rest.trim().to_string())
    } else {
        TimelineItem::System(trimmed.to_string())
    }
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
        assert!(loaded[0].session.name.starts_with("hist-20260510"));
        assert_eq!(loaded[0].history_info.len(), 3);
        assert_eq!(loaded[0].messages.len(), 1);
        assert!(matches!(
            loaded[0].session.timeline[1],
            TimelineItem::User(_)
        ));
        assert!(matches!(
            loaded[0].session.timeline[2],
            TimelineItem::Assistant(_)
        ));
    }
}
