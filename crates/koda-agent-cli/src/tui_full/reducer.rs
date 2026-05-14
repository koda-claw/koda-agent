use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use koda_agent_core::{AgentConfig, AgentRuntime};
use ratatui::layout::Rect;
use std::collections::BTreeMap;

use super::build_runtime;
use super::render::{
    max_timeline_scroll_for_width, timeline_content_width, timeline_viewport_lines, trim_chars,
};
use super::state::{
    FocusPane, Overlay, PendingAsk, SessionStatus, StreamMetrics, StreamState, ThinkingState,
    TimelineItem, TuiAppState, TuiSessionState, UsageStats,
};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum KeyAction {
    None,
    Quit,
    Submit(String),
    NewSession,
    Abort,
    Local(LocalCommand),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LocalCommand {
    Branch(Option<String>),
    Clear,
    Close,
    Commands,
    Help,
    CancelAsk,
    Rename(String),
    Sessions,
    Switch(String),
}

pub(super) fn reduce_key_event(state: &mut TuiAppState, key: KeyEvent) -> KeyAction {
    // Ignore key release events to prevent double-input on Windows.
    if key.kind == KeyEventKind::Release {
        return KeyAction::None;
    }
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
        return KeyAction::Quit;
    }
    if state.overlay != Overlay::None {
        match (key.modifiers, key.code) {
            (_, KeyCode::Esc) | (_, KeyCode::Char('?')) => {
                state.overlay = Overlay::None;
                state.status = "closed overlay".into();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                state.overlay = Overlay::None;
                state.status = "closed command palette".into();
            }
            (_, KeyCode::Char(ch)) if state.overlay == Overlay::Commands => {
                if let Some(command) = command_template_for_digit(ch) {
                    state.composer = command.to_string();
                    state.overlay = Overlay::None;
                    state.focus = FocusPane::Composer;
                    state.status = "inserted command template".into();
                }
            }
            _ => {}
        }
        return KeyAction::None;
    }
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) => {
            return KeyAction::Quit;
        }
        (_, KeyCode::Char('?')) if state.composer.is_empty() => {
            state.overlay = Overlay::Help;
            state.status = "opened help".into();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
            state.overlay = Overlay::Commands;
            state.status = "opened command palette".into();
        }
        (_, KeyCode::F(1)) => {
            state.overlay = Overlay::Help;
            state.status = "opened help".into();
        }
        (_, KeyCode::F(2)) => {
            state.overlay = Overlay::Commands;
            state.status = "opened command palette".into();
        }
        (_, KeyCode::F(3)) if state.composer.is_empty() => return KeyAction::NewSession,
        (_, KeyCode::F(4)) if state.composer.is_empty() => {
            return KeyAction::Local(LocalCommand::Branch(None));
        }
        (_, KeyCode::F(5)) if state.composer.is_empty() => {
            return KeyAction::Local(LocalCommand::Clear);
        }
        (_, KeyCode::F(6)) if state.composer.is_empty() => {
            return KeyAction::Local(LocalCommand::Close);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('s')) => {
            return KeyAction::Abort;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
            state.composer.push('\n');
        }
        (KeyModifiers::CONTROL, KeyCode::Char('n')) if state.composer.is_empty() => {
            return KeyAction::NewSession;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('b')) if state.composer.is_empty() => {
            return KeyAction::Local(LocalCommand::Branch(None));
        }
        (KeyModifiers::CONTROL, KeyCode::Char('l')) if state.composer.is_empty() => {
            return KeyAction::Local(LocalCommand::Clear);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('w')) if state.composer.is_empty() => {
            return KeyAction::Local(LocalCommand::Close);
        }
        (_, KeyCode::Enter) => {
            let prompt = state.composer.trim().to_string();
            if prompt.is_empty() {
                if active_pending_ask(state).is_some() {
                    state.status = "请输入回答，或按数字选择候选项".into();
                } else {
                    state.status = "composer is empty".into();
                }
            } else if let Some(answer) = parse_pending_ask_prompt(state, &prompt) {
                state.composer.clear();
                clear_active_pending_ask(state);
                return KeyAction::Submit(answer);
            } else if let Some(command) = parse_local_command(&prompt) {
                state.composer.clear();
                return KeyAction::Local(command);
            } else if active_pending_ask(state).is_some() && prompt.starts_with('/') {
                state.status =
                    "ask_user command not recognized; use /answer <text>, /choose <n>, or /cancel"
                        .into();
            } else if state
                .active_session()
                .is_some_and(|s| matches!(s.status, SessionStatus::Running))
            {
                state.status = format!("session #{} is already running", state.active);
            } else {
                state.composer.clear();
                clear_active_pending_ask(state);
                return KeyAction::Submit(prompt);
            }
        }
        (_, KeyCode::Backspace) => {
            state.composer.pop();
        }
        (_, KeyCode::Tab) => {
            state.focus = match state.focus {
                FocusPane::Composer => FocusPane::Timeline,
                FocusPane::Timeline => FocusPane::Inspector,
                FocusPane::Inspector => FocusPane::Sessions,
                FocusPane::Sessions => FocusPane::Composer,
            };
        }
        (_, KeyCode::Char('q')) if state.composer.is_empty() => return KeyAction::Quit,
        (_, KeyCode::Char('n')) if state.composer.is_empty() => return KeyAction::NewSession,
        (_, KeyCode::Char(ch)) if state.composer.is_empty() && ch.is_ascii_digit() => {
            if let Some(answer) = pending_ask_choice(state, ch) {
                clear_active_pending_ask(state);
                return KeyAction::Submit(answer);
            }
        }
        (_, KeyCode::PageDown) => {
            scroll_active_timeline(state, 10);
        }
        (_, KeyCode::PageUp) => {
            scroll_active_timeline(state, -10);
        }
        (_, KeyCode::Home) => {
            let active = state.active;
            if let Some(session) = state.active_session_mut() {
                session.timeline_scroll = 0;
                session.timeline_follow_tail = false;
                state.status = format!("session #{} timeline top | follow=off", active);
            }
        }
        (_, KeyCode::End) => {
            scroll_active_timeline_to_bottom(state);
        }
        (_, KeyCode::Char('f')) if state.composer.is_empty() => {
            if let Some(session) = state.sessions.get_mut(&state.active) {
                session.fold = !session.fold;
                session.mark_timeline_dirty();
                state.status = format!(
                    "session #{} fold={}",
                    session.id,
                    if session.fold { "on" } else { "off" }
                );
            }
        }
        (_, KeyCode::Down) => {
            if let Some(next) = state
                .sessions
                .range((state.active + 1)..)
                .next()
                .map(|(id, _)| *id)
                .or_else(|| state.sessions.keys().next().copied())
            {
                activate_session(state, next);
            }
        }
        (_, KeyCode::Up) => {
            if let Some(prev) = state
                .sessions
                .range(..state.active)
                .next_back()
                .map(|(id, _)| *id)
                .or_else(|| state.sessions.keys().next_back().copied())
            {
                activate_session(state, prev);
            }
        }
        (_, KeyCode::Char(ch))
            if state.focus == FocusPane::Composer
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
        {
            state.composer.push(ch);
        }
        _ => {}
    }
    KeyAction::None
}

pub(super) fn reduce_mouse_event(state: &mut TuiAppState, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::Down(_) => focus_pane_at(state, mouse.column, mouse.row),
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
            if pane_at(state, mouse.column, mouse.row) == Some(FocusPane::Inspector) =>
        {
            state.focus = FocusPane::Inspector;
            state.status = "Inspector is fixed; scroll the Timeline to review output".into();
        }
        MouseEventKind::ScrollDown => scroll_active_timeline(state, 3),
        MouseEventKind::ScrollUp => scroll_active_timeline(state, -3),
        _ => {}
    }
}

fn activate_session(state: &mut TuiAppState, id: usize) {
    state.active = id;
    if let Some(session) = state.active_session_mut() {
        session.unread_events = 0;
        session.timeline_unseen = 0;
    }
    state.status = format!("switched to session #{id}");
}

fn focus_pane_at(state: &mut TuiAppState, column: u16, row: u16) {
    if let Some(pane) = pane_at(state, column, row) {
        if pane == FocusPane::Sessions {
            select_session_at_row(state, row);
        }
        state.focus = pane;
    }
}

fn select_session_at_row(state: &mut TuiAppState, row: u16) {
    let Some(area) = state.last_layout.and_then(|layout| layout.sidebar) else {
        return;
    };
    let Some(index) = row.checked_sub(area.y.saturating_add(1)).map(usize::from) else {
        return;
    };
    let Some(id) = state.sessions.keys().nth(index).copied() else {
        return;
    };
    activate_session(state, id);
}

fn pane_at(state: &TuiAppState, column: u16, row: u16) -> Option<FocusPane> {
    let layout = state.last_layout?;
    if rect_contains(layout.composer, column, row) {
        Some(FocusPane::Composer)
    } else if rect_contains(layout.timeline, column, row) {
        Some(FocusPane::Timeline)
    } else if layout
        .inspector
        .is_some_and(|area| rect_contains(area, column, row))
    {
        Some(FocusPane::Inspector)
    } else if layout
        .sidebar
        .is_some_and(|area| rect_contains(area, column, row))
    {
        Some(FocusPane::Sessions)
    } else {
        None
    }
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn scroll_active_timeline(state: &mut TuiAppState, delta: i16) {
    let active = state.active;
    let viewport = timeline_viewport_lines(state);
    let width = timeline_content_width(state);
    if let Some(session) = state.active_session_mut() {
        let max_scroll = max_timeline_scroll_for_width(session, viewport, width);
        if delta >= 0 {
            session.timeline_scroll = session.timeline_scroll.saturating_add(delta as u16);
        } else {
            session.timeline_scroll = session.timeline_scroll.saturating_sub(delta.unsigned_abs());
            session.timeline_follow_tail = false;
        }
        session.timeline_scroll = session.timeline_scroll.min(max_scroll);
        if delta >= 0 && session.timeline_scroll >= max_scroll {
            session.timeline_follow_tail = true;
            session.timeline_unseen = 0;
        } else if delta != 0 {
            session.timeline_follow_tail = false;
        }
        state.status = format!(
            "session #{} timeline scroll={}/{} follow={}",
            active,
            session.timeline_scroll,
            max_scroll,
            if session.timeline_follow_tail {
                "on"
            } else {
                "off"
            }
        );
    }
}

fn scroll_active_timeline_to_bottom(state: &mut TuiAppState) {
    let active = state.active;
    let viewport = timeline_viewport_lines(state);
    let width = timeline_content_width(state);
    if let Some(session) = state.active_session_mut() {
        let max_scroll = max_timeline_scroll_for_width(session, viewport, width);
        session.timeline_scroll = max_scroll;
        session.timeline_follow_tail = true;
        session.timeline_unseen = 0;
        state.status = format!("session #{active} timeline bottom | follow=on");
    }
}

pub(super) fn parse_local_command(input: &str) -> Option<LocalCommand> {
    let text = input.trim();
    if !text.starts_with('/') {
        return None;
    }
    let mut parts = text.splitn(2, char::is_whitespace);
    let command = parts.next()?.to_ascii_lowercase();
    let arg = parts.next().unwrap_or_default().trim();
    match command.as_str() {
        "/branch" => Some(LocalCommand::Branch(
            (!arg.is_empty()).then(|| arg.to_string()),
        )),
        "/clear" => Some(LocalCommand::Clear),
        "/close" => Some(LocalCommand::Close),
        "/cancel" => Some(LocalCommand::CancelAsk),
        "/commands" | "/palette" => Some(LocalCommand::Commands),
        "/help" => Some(LocalCommand::Help),
        "/rename" if !arg.is_empty() => Some(LocalCommand::Rename(arg.to_string())),
        "/rename" => Some(LocalCommand::Sessions),
        "/sessions" => Some(LocalCommand::Sessions),
        "/switch" if !arg.is_empty() => Some(LocalCommand::Switch(arg.to_string())),
        "/switch" => Some(LocalCommand::Sessions),
        _ => None,
    }
}

pub(super) fn apply_local_command(
    state: &mut TuiAppState,
    runtimes: &mut BTreeMap<usize, AgentRuntime>,
    command: LocalCommand,
    cfg: &AgentConfig,
) -> Result<()> {
    match command {
        LocalCommand::Branch(name) => branch_active_session(state, runtimes, name, cfg),
        LocalCommand::Clear => {
            if let Some(session) = state.active_session_mut() {
                session.clear_timeline();
                session.timeline_scroll = 0;
                session.timeline_follow_tail = true;
                session.timeline_unseen = 0;
                session.last_error = None;
                session.active_turn = None;
                session.last_tool = None;
                session.pending_ask = None;
                session.usage = UsageStats::default();
                session.stream_state = StreamState::Idle;
                session.thinking_state = ThinkingState::Unavailable;
                session.stream_metrics = StreamMetrics::default();
                session.unread_events = 0;
                session.last_notice = Some("timeline cleared".into());
                session.push_timeline(TimelineItem::System("Timeline cleared.".into()));
                state.status = format!("cleared session #{}", state.active);
            }
            Ok(())
        }
        LocalCommand::CancelAsk => {
            if let Some(session) = state.active_session_mut() {
                if session.pending_ask.take().is_some() {
                    session.status = SessionStatus::Idle;
                    session.last_notice = Some("ask_user cancelled".into());
                    session
                        .push_timeline(TimelineItem::System("已取消当前 ask_user 等待。".into()));
                    state.status =
                        format!("cancelled pending ask_user for session #{}", state.active);
                } else {
                    state.status = "no pending ask_user to cancel".into();
                }
            }
            Ok(())
        }
        LocalCommand::Close => {
            close_active_session(state, runtimes);
            Ok(())
        }
        LocalCommand::Commands => {
            state.overlay = Overlay::Commands;
            state.status = "opened command palette".into();
            Ok(())
        }
        LocalCommand::Help => {
            state.overlay = Overlay::Help;
            state.status = "opened help".into();
            Ok(())
        }
        LocalCommand::Rename(name) => {
            if let Some(session) = state.active_session_mut() {
                session.name = trim_chars(name.trim(), 32);
                state.status = format!("renamed session #{}", state.active);
            }
            Ok(())
        }
        LocalCommand::Sessions => {
            let summary = state
                .sessions
                .values()
                .map(|s| format!("#{} {} [{}]", s.id, s.name, s.status.label()))
                .collect::<Vec<_>>()
                .join(" | ");
            if let Some(session) = state.active_session_mut() {
                session.push_timeline(TimelineItem::System(format!("Sessions: {summary}")));
            }
            state.status = "rendered session list".into();
            Ok(())
        }
        LocalCommand::Switch(target) => {
            switch_session(state, &target);
            Ok(())
        }
    }
}

fn command_template_for_digit(ch: char) -> Option<&'static str> {
    match ch {
        '1' => Some("/branch "),
        '2' => Some("/switch "),
        '3' => Some("/rename "),
        '4' => Some("/sessions"),
        '5' => Some("/clear"),
        '6' => Some("/close"),
        '7' => Some("/status"),
        '8' => Some("/llms"),
        '9' => Some("/continue"),
        _ => None,
    }
}

fn branch_active_session(
    state: &mut TuiAppState,
    runtimes: &mut BTreeMap<usize, AgentRuntime>,
    name: Option<String>,
    cfg: &AgentConfig,
) -> Result<()> {
    let active = state.active;
    if state
        .active_session()
        .is_some_and(|s| matches!(s.status, SessionStatus::Running))
    {
        state.status = format!("cannot branch running session #{active}");
        return Ok(());
    }
    let old = state.active_session().cloned();
    let Some(old) = old else {
        state.status = format!("missing session #{active}");
        return Ok(());
    };
    let id = state.next_id;
    state.next_id += 1;
    let branch_name = name.unwrap_or_else(|| format!("{}-branch", old.name));
    state.sessions.insert(
        id,
        TuiSessionState {
            id,
            name: trim_chars(&branch_name, 32),
            status: SessionStatus::Idle,
            timeline: old
                .timeline
                .into_iter()
                .chain([TimelineItem::System(format!(
                    "Branched from session #{active}."
                ))])
                .collect(),
            fold: old.fold,
            last_error: None,
            active_turn: None,
            last_tool: old.last_tool.clone(),
            pending_ask: old.pending_ask.clone(),
            unread_events: 0,
            completed_tasks: old.completed_tasks,
            failed_tasks: old.failed_tasks,
            last_notice: Some(format!("branched from session #{active}")),
            timeline_scroll: 0,
            timeline_follow_tail: true,
            timeline_unseen: 0,
            timeline_revision: old.timeline_revision.wrapping_add(1),
            timeline_cache: None,
            usage: old.usage.clone(),
            stream_state: StreamState::Idle,
            thinking_state: old.thinking_state.clone(),
            stream_metrics: StreamMetrics::default(),
            session_started_at: None,
            turn_started_at: None,
            last_turn_elapsed: None,
        },
    );
    let runtime = runtimes
        .get(&active)
        .map(AgentRuntime::fork_session)
        .map(Ok)
        .unwrap_or_else(|| build_runtime(cfg.clone()))?;
    runtimes.insert(id, runtime);
    state.active = id;
    state.status = format!("branched session #{active} -> #{id}");
    Ok(())
}

fn active_pending_ask(state: &TuiAppState) -> Option<&PendingAsk> {
    state
        .active_session()
        .and_then(|session| session.pending_ask.as_ref())
}

fn clear_active_pending_ask(state: &mut TuiAppState) {
    if let Some(session) = state.active_session_mut() {
        session.pending_ask = None;
    }
}

fn parse_pending_ask_prompt(state: &TuiAppState, prompt: &str) -> Option<String> {
    let ask = active_pending_ask(state)?;
    let trimmed = prompt.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    match parts
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "/answer" => {
            let answer = parts.next().unwrap_or_default().trim();
            (!answer.is_empty()).then(|| answer.to_string())
        }
        "/choose" => parts
            .next()
            .unwrap_or_default()
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|n| ask.candidates.get(n.saturating_sub(1)).cloned()),
        _ if trimmed.starts_with('/') => None,
        _ => Some(trimmed.to_string()),
    }
}

fn pending_ask_choice(state: &TuiAppState, ch: char) -> Option<String> {
    let ask = active_pending_ask(state)?;
    let idx = ch.to_digit(10)? as usize;
    if idx == 0 {
        return None;
    }
    ask.candidates.get(idx - 1).cloned()
}

fn close_active_session(state: &mut TuiAppState, runtimes: &mut BTreeMap<usize, AgentRuntime>) {
    if state.sessions.len() <= 1 {
        state.status = "cannot close the last session".into();
        return;
    }
    let active = state.active;
    if state
        .active_session()
        .is_some_and(|s| matches!(s.status, SessionStatus::Running))
    {
        state.status = format!("cannot close running session #{active}; stop it first");
        return;
    }
    state.sessions.remove(&active);
    runtimes.remove(&active);
    let fallback = state.sessions.keys().next().copied().unwrap_or(1);
    state.active = fallback;
    if let Some(session) = state.active_session_mut() {
        session.unread_events = 0;
        session.timeline_unseen = 0;
    }
    state.status = format!("closed session #{active}; switched to #{fallback}");
}

pub(super) fn switch_session(state: &mut TuiAppState, target: &str) {
    let target = target.trim();
    let found = target
        .parse::<usize>()
        .ok()
        .filter(|id| state.sessions.contains_key(id))
        .or_else(|| {
            state
                .sessions
                .iter()
                .find_map(|(id, s)| (s.name == target).then_some(*id))
        });
    if let Some(id) = found {
        activate_session(state, id);
    } else {
        state.status = format!("no session found for {target}");
    }
}
