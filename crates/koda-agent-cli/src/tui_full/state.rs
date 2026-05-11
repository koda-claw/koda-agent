use koda_agent_core::{AgentConfig, LlmUsageSummary};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug)]
pub(super) struct AppLayout {
    pub(super) header: Rect,
    pub(super) sidebar: Option<Rect>,
    pub(super) timeline: Rect,
    pub(super) inspector: Option<Rect>,
    pub(super) composer: Rect,
    pub(super) status: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FocusPane {
    Composer,
    Timeline,
    Sessions,
    Inspector,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LayoutMode {
    Wide,
    Medium,
    Narrow,
}

#[derive(Clone, Debug)]
pub(super) struct TuiAppState {
    pub(super) root_label: String,
    pub(super) model_label: String,
    pub(super) api_mode: String,
    pub(super) stream_enabled: bool,
    pub(super) mouse_capture: bool,
    pub(super) sessions: BTreeMap<usize, TuiSessionState>,
    pub(super) active: usize,
    pub(super) next_id: usize,
    pub(super) focus: FocusPane,
    pub(super) layout_mode: LayoutMode,
    pub(super) status: String,
    pub(super) composer: String,
    pub(super) overlay: Overlay,
    pub(super) last_layout: Option<AppLayout>,
    pub(super) tick: u64,
}

impl TuiAppState {
    pub(super) fn from_config(cfg: &AgentConfig) -> Self {
        let mut sessions = BTreeMap::new();
        sessions.insert(
            1,
            TuiSessionState {
                id: 1,
                name: "main".into(),
                status: SessionStatus::Idle,
                timeline: vec![
                    TimelineItem::System(
                        "Full-screen TUI preview. Current `koda-agent tui` line mode is unchanged."
                            .into(),
                    ),
                    TimelineItem::Assistant(
                        "Next slices will wire live AgentRuntime events, structured tool cards, scrollback, and command palette.".into(),
                    ),
                ],
                fold: true,
                last_error: None,
                active_turn: None,
                last_tool: None,
                unread_events: 0,
                completed_tasks: 0,
                failed_tasks: 0,
                last_notice: None,
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
        );
        Self {
            root_label: cfg.root_dir.display().to_string(),
            model_label: cfg.openai_model.clone(),
            api_mode: cfg.llm_api_style.clone(),
            stream_enabled: cfg.stream,
            mouse_capture: true,
            sessions,
            active: 1,
            next_id: 2,
            focus: FocusPane::Composer,
            layout_mode: LayoutMode::Wide,
            status: "ready | Enter submit | Ctrl-S stop | Ctrl-Q/Esc quit | Tab focus".into(),
            composer: String::new(),
            overlay: Overlay::None,
            last_layout: None,
            tick: 0,
        }
    }

    pub(super) fn active_session(&self) -> Option<&TuiSessionState> {
        self.sessions.get(&self.active)
    }

    pub(super) fn active_session_mut(&mut self) -> Option<&mut TuiSessionState> {
        self.sessions.get_mut(&self.active)
    }

    pub(super) fn create_session(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.sessions.insert(
            id,
            TuiSessionState {
                id,
                name: format!("agent-{id}"),
                status: SessionStatus::Idle,
                timeline: vec![TimelineItem::System("New runtime session is ready.".into())],
                fold: true,
                last_error: None,
                active_turn: None,
                last_tool: None,
                unread_events: 0,
                completed_tasks: 0,
                failed_tasks: 0,
                last_notice: None,
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
        );
        self.active = id;
        id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Overlay {
    None,
    Help,
    Commands,
}

#[derive(Clone, Debug)]
pub(super) struct TuiSessionState {
    pub(super) id: usize,
    pub(super) name: String,
    pub(super) status: SessionStatus,
    pub(super) timeline: Vec<TimelineItem>,
    pub(super) fold: bool,
    pub(super) last_error: Option<String>,
    pub(super) active_turn: Option<usize>,
    pub(super) last_tool: Option<ToolDetail>,
    pub(super) unread_events: u32,
    pub(super) completed_tasks: u32,
    pub(super) failed_tasks: u32,
    pub(super) last_notice: Option<String>,
    pub(super) timeline_scroll: u16,
    pub(super) timeline_follow_tail: bool,
    pub(super) timeline_unseen: u32,
    pub(super) timeline_revision: u64,
    pub(super) timeline_cache: Option<TimelineRenderCache>,
    pub(super) usage: UsageStats,
    pub(super) stream_state: StreamState,
    pub(super) thinking_state: ThinkingState,
    pub(super) stream_metrics: StreamMetrics,
}

impl TuiSessionState {
    pub(super) fn push_timeline(&mut self, item: TimelineItem) {
        self.timeline.push(item);
        self.mark_timeline_dirty();
    }

    pub(super) fn clear_timeline(&mut self) {
        self.timeline.clear();
        self.mark_timeline_dirty();
    }

    pub(super) fn mark_timeline_dirty(&mut self) {
        self.timeline_revision = self.timeline_revision.wrapping_add(1);
        self.timeline_cache = None;
    }
}

#[derive(Clone, Debug)]
pub(super) struct TimelineRenderCache {
    pub(super) revision: u64,
    pub(super) width: usize,
    pub(super) fold: bool,
    pub(super) signature: TimelineSignature,
    pub(super) lines: Vec<Line<'static>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TimelineSignature {
    pub(super) items: usize,
    pub(super) text_len: usize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct StreamMetrics {
    pub(super) content_chunks: u32,
    pub(super) thinking_chunks: u32,
    pub(super) usage_chunks: u32,
    pub(super) last_delta_tick: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct UsageStats {
    pub(super) current_turn: Option<LlmUsageSummary>,
    pub(super) session_input_tokens: u64,
    pub(super) session_output_tokens: u64,
    pub(super) session_total_tokens: u64,
    pub(super) session_cached_tokens: u64,
    pub(super) events: u32,
    pub(super) unavailable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StreamState {
    Idle,
    Streaming,
    FinalOnly,
}

impl StreamState {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Streaming => "streaming",
            Self::FinalOnly => "final-only",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ThinkingState {
    Unavailable,
    Streaming,
    FinalOnly,
}

impl ThinkingState {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Streaming => "streaming",
            Self::FinalOnly => "final-only",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ToolDetail {
    pub(super) turn: usize,
    pub(super) index: usize,
    pub(super) name: String,
    pub(super) args: String,
    pub(super) result: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) enum SessionStatus {
    Idle,
    Running,
    Error,
}

impl SessionStatus {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Error => "error",
        }
    }

    pub(super) fn style(&self) -> Style {
        match self {
            Self::Idle => Style::default().fg(Color::Gray),
            Self::Running => Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
            Self::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum TimelineItem {
    User(String),
    Assistant(String),
    Thinking(String),
    ToolCall {
        name: String,
        args: String,
    },
    ToolResult {
        name: String,
        args: String,
        data: String,
    },
    System(String),
    Error(String),
}
