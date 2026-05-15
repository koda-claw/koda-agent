use anyhow::{Context, Result, bail};
mod history;
mod markdown;
mod reducer;
mod render;
mod state;
mod tool_cards;

use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use history::load_recent_history_sessions;
use koda_agent_core::{AgentConfig, AgentEvent, AgentRuntime};
use koda_agent_llm::OpenAiClient;
use koda_agent_tools::GenericToolDispatcher;
use ratatui::{Terminal, backend::CrosstermBackend};
use reducer::{KeyAction, apply_local_command, reduce_key_event, reduce_mouse_event};
#[cfg(test)]
use reducer::{LocalCommand, parse_local_command, switch_session};
#[cfg(test)]
use render::summarize_tool_result;
use render::{
    max_timeline_scroll_for_width, render_app, timeline_content_width, timeline_viewport_lines,
    trim_chars,
};
#[cfg(test)]
use state::LayoutMode;
use state::{
    FocusPane, Overlay, PendingAsk, SessionStatus, StreamState, ThinkingState, TimelineItem,
    ToolDetail, TuiAppState, TuiSessionState,
};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, IsTerminal, Stdout, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
#[cfg(test)]
use tool_cards::{render_tool_call_card, render_tool_result_card};

#[cfg(test)]
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseEvent, MouseEventKind,
};

const MAX_TIMELINE_ITEMS: usize = 2_000;
const MAX_RUNTIME_EVENTS_PER_FRAME: usize = 1;
const MIN_STREAM_FRAME: Duration = Duration::from_millis(24);
const MAX_TERMINAL_EVENTS_PER_FRAME: usize = 64;

pub(crate) async fn run_tui_full(cfg: AgentConfig) -> Result<()> {
    ensure_interactive_terminal()?;
    let _guard = TerminalModeGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let mut state = TuiAppState::from_config(&cfg);
    let (tx, rx) = mpsc::unbounded_channel();
    let mut runtimes = BTreeMap::new();
    runtimes.insert(1, build_runtime(cfg.clone())?);
    load_tui_history_sessions(&cfg, &mut state, &mut runtimes)?;
    let res = run_event_loop(&mut terminal, &mut state, &mut runtimes, tx, rx, cfg).await;
    terminal.show_cursor()?;
    res
}

fn build_runtime(cfg: AgentConfig) -> Result<AgentRuntime> {
    let llm = OpenAiClient::multi_arc(cfg.clone());
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    AgentRuntime::new(cfg, llm, tools)
}

fn load_tui_history_sessions(
    cfg: &AgentConfig,
    state: &mut TuiAppState,
    runtimes: &mut BTreeMap<usize, AgentRuntime>,
) -> Result<()> {
    let loaded = load_recent_history_sessions(cfg, state.next_id);
    if loaded.is_empty() {
        return Ok(());
    }
    for loaded_session in loaded {
        let id = loaded_session.session.id;
        let runtime = build_runtime(cfg.clone())?;
        runtime.restore_session_snapshot(loaded_session.history_info, loaded_session.messages);
        state.sessions.insert(id, loaded_session.session);
        runtimes.insert(id, runtime);
        state.next_id = state.next_id.max(id + 1);
    }
    state.status = format!(
        "ready | loaded {} historical sessions | Enter submit | F1 help",
        state.next_id.saturating_sub(2)
    );
    Ok(())
}

fn ensure_interactive_terminal() -> Result<()> {
    let term = env::var("TERM").unwrap_or_default();
    if term.eq_ignore_ascii_case("dumb")
        || !io::stdout().is_terminal()
        || !io::stdin().is_terminal()
    {
        bail!(
            "full-screen TUI requires an interactive terminal; use `koda-agent tui --line`, `koda-agent --input <prompt>`, or `koda-agent --task <iodir>`"
        );
    }
    Ok(())
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut TuiAppState,
    runtimes: &mut BTreeMap<usize, AgentRuntime>,
    tx: mpsc::UnboundedSender<TuiRuntimeEvent>,
    mut rx: mpsc::UnboundedReceiver<TuiRuntimeEvent>,
    mut cfg: AgentConfig,
) -> Result<()> {
    let tick_rate = Duration::from_millis(33);
    let mut last_tick = Instant::now();
    let mut last_runtime_drain = Instant::now() - MIN_STREAM_FRAME;
    terminal.draw(|frame| render_app(frame, state))?;

    // Drain any buffered terminal events (e.g. on macOS after entering raw mode)
    // to prevent the first real key event from being consumed by stale data.
    while event::poll(Duration::ZERO)? {
        event::read()?;
    }

    loop {
        let mut dirty = false;
        let runtime_budget = if last_runtime_drain.elapsed() >= MIN_STREAM_FRAME {
            MAX_RUNTIME_EVENTS_PER_FRAME
        } else {
            0
        };
        for _ in 0..runtime_budget {
            let Ok(event) = rx.try_recv() else {
                break;
            };
            apply_runtime_event(state, event);
            last_runtime_drain = Instant::now();
            dirty = true;
        }
        let timeout = if dirty {
            Duration::ZERO
        } else {
            tick_rate.saturating_sub(last_tick.elapsed())
        };
        if event::poll(timeout)? {
            for idx in 0..MAX_TERMINAL_EVENTS_PER_FRAME {
                if idx > 0 && !event::poll(Duration::ZERO)? {
                    break;
                }
                let event = event::read()?;
                if handle_terminal_event(state, runtimes, tx.clone(), &cfg, event)? {
                    return Ok(());
                }
                // Consume pending_model_switch (set by /model command)
                if let Some(new_model) = state.pending_model_switch.take() {
                    let available: Vec<String> =
                        cfg.llm_configs.iter().map(|m| m.name.clone()).collect();
                    match cfg.llm_configs.iter().find(|m| m.name == new_model) {
                        Some(model_cfg) => {
                            cfg.openai_model = new_model.clone();
                            cfg.llm_api_style = model_cfg.api_style.clone();
                            state.model_label = new_model.clone();
                            state.api_mode = cfg.llm_api_style.clone();
                            if let Some(session) = state.active_session_mut() {
                                let msg = "Model switched to ".to_string()
                                    + &new_model
                                    + ". Use /branch to start a session with the new model.";
                                session.push_timeline(TimelineItem::System(msg));
                            }
                            state.status = "model switched to ".to_string() + &new_model;
                        }
                        None => {
                            let hint = "Unknown model: '".to_string()
                                + &new_model
                                + "'. Available: "
                                + &available.join(", ");
                            state.status = hint.clone();
                            if let Some(session) = state.active_session_mut() {
                                session.push_timeline(TimelineItem::System(hint));
                            }
                        }
                    }
                }
                dirty = true;
            }
        }
        if last_tick.elapsed() >= tick_rate {
            state.tick = state.tick.saturating_add(1);
            last_tick = Instant::now();
            dirty = true;
        }
        if dirty {
            terminal.draw(|frame| render_app(frame, state))?;
        }
    }
}

fn handle_terminal_event(
    state: &mut TuiAppState,
    runtimes: &mut BTreeMap<usize, AgentRuntime>,
    tx: mpsc::UnboundedSender<TuiRuntimeEvent>,
    cfg: &AgentConfig,
    event: Event,
) -> Result<bool> {
    match event {
        Event::Key(key) => match reduce_key_event(state, key) {
            KeyAction::Quit => return Ok(true),
            KeyAction::Submit(prompt) => submit_active_task(state, runtimes, tx, prompt),
            KeyAction::NewSession => {
                let id = state.create_session();
                runtimes.insert(id, build_runtime(cfg.clone())?);
                state.status = format!("created session #{id}");
            }
            KeyAction::Local(command) => {
                apply_local_command(state, runtimes, command, cfg)?;
            }
            KeyAction::Abort => {
                if let Some(runtime) = runtimes.get(&state.active) {
                    runtime.abort();
                    state.status = format!("stop requested for session #{}", state.active);
                }
            }
            KeyAction::None => {}
        },
        Event::Paste(text)
            if state.focus == FocusPane::Composer && state.overlay == Overlay::None =>
        {
            // Filter control chars except \n \r \t; convert tabs to spaces
            let cleaned: String = text
                .chars()
                .map(|ch| match ch {
                    '\n' | '\r' => ch,
                    '\t' => ' ',
                    c if c.is_control() => '\0',
                    c => c,
                })
                .filter(|&c| c != '\0')
                .collect();
            // Normalize line endings
            let cleaned = cleaned.replace("\r\n", "\n").replace('\r', "\n");
            // Insert line by line into composer
            for (i, line) in cleaned.split('\n').enumerate() {
                if i > 0 {
                    state.composer.insert_newline();
                }
                if !line.is_empty() {
                    state.composer.insert_str(line);
                }
            }
        }
        Event::Mouse(mouse) => reduce_mouse_event(state, mouse),
        _ => {}
    }
    Ok(false)
}

struct TerminalModeGuard;

impl TerminalModeGuard {
    fn enter() -> Result<Self> {
        reset_terminal_mouse_modes();
        enable_raw_mode().context("enable terminal raw mode")?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            Hide
        )
        .context("enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        reset_terminal_mouse_modes();
        let _ = execute!(
            io::stdout(),
            Show,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        reset_terminal_mouse_modes();
        let _ = disable_raw_mode();
    }
}

fn reset_terminal_mouse_modes() {
    // Some terminals can be left in SGR mouse mode after an abrupt TUI exit; when
    // that happens, wheel events are printed as `^[[<64;...M` in the shell.
    let mut out = io::stdout();
    let _ = out.write_all(
        b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1004l",
    );
    let _ = out.flush();
}

#[derive(Debug)]
enum TuiRuntimeEvent {
    Agent {
        session_id: usize,
        event: AgentEvent,
    },
    Done {
        session_id: usize,
        usage: Option<koda_agent_core::LlmUsageSummary>,
    },
    Failed {
        session_id: usize,
        error: String,
    },
}

fn submit_active_task(
    state: &mut TuiAppState,
    runtimes: &BTreeMap<usize, AgentRuntime>,
    tx: mpsc::UnboundedSender<TuiRuntimeEvent>,
    prompt: String,
) {
    let session_id = state.active;
    let Some(runtime) = runtimes.get(&session_id).cloned() else {
        state.status = format!("missing runtime for session #{session_id}");
        return;
    };
    if let Some(session) = state.active_session_mut() {
        if should_auto_name_session(session) {
            session.name = prompt_session_title(&prompt);
        }
        session.status = SessionStatus::Running;
        session.last_error = None;
        session.active_turn = None;
        session.unread_events = 0;
        session.last_notice = Some("task submitted".into());
        session.usage.current_turn = None;
        session.usage.unavailable = false;
        session.stream_metrics = Default::default();
        session.session_started_at = Some(std::time::Instant::now());
        session.turn_started_at = None;
        session.last_turn_elapsed = None;
        session.push_timeline(TimelineItem::User(prompt.clone()));
        session.timeline_follow_tail = true;
        session.timeline_unseen = 0;
    }
    state.status = format!("session #{session_id} running");
    let logs_dir = runtime.config().logs_dir.clone();
    tokio::spawn(async move {
        let event_tx = tx.clone();
        let result = runtime
            .put_task_with_events(prompt, move |event| {
                let _ = event_tx.send(TuiRuntimeEvent::Agent { session_id, event });
            })
            .await;
        match result {
            Ok(_output) => {
                let usage = latest_usage_from_log(&logs_dir);
                let _ = tx.send(TuiRuntimeEvent::Done { session_id, usage });
            }
            Err(error) => {
                let _ = tx.send(TuiRuntimeEvent::Failed {
                    session_id,
                    error: format!("{error:#}"),
                });
            }
        }
    });
}

fn should_auto_name_session(session: &TuiSessionState) -> bool {
    session
        .timeline
        .iter()
        .all(|item| matches!(item, TimelineItem::System(_) | TimelineItem::Assistant(_)))
        && (session.name == "main"
            || session.name == format!("agent-{}", session.id)
            || session.name == format!("new-{}", session.id))
}

fn prompt_session_title(prompt: &str) -> String {
    let compact = prompt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|ch: char| ch == '"' || ch == '\'' || ch.is_ascii_punctuation())
        .to_string();
    let title = if compact.is_empty() {
        "untitled".to_string()
    } else {
        compact
    };
    trim_chars(&title, 32)
}

fn apply_runtime_event(state: &mut TuiAppState, event: TuiRuntimeEvent) {
    match event {
        TuiRuntimeEvent::Agent { session_id, event } => {
            let is_active = session_id == state.active;
            let viewport = timeline_viewport_lines(state);
            let width = timeline_content_width(state);
            if let Some(session) = state.sessions.get_mut(&session_id) {
                apply_agent_event(session, &event, state.tick);
                reconcile_session_timeline_scroll(session, viewport, width, is_active);
                if is_active {
                    state.status = format_active_timeline_status(
                        session_id,
                        short_event_label(&event),
                        session,
                    );
                } else {
                    session.unread_events = session.unread_events.saturating_add(1);
                    session.last_notice = Some(short_event_label(&event).into());
                    state.status = format!(
                        "background session #{session_id}: {}",
                        short_event_label(&event)
                    );
                }
            }
        }
        TuiRuntimeEvent::Done { session_id, usage } => {
            let is_active = session_id == state.active;
            let viewport = timeline_viewport_lines(state);
            let width = timeline_content_width(state);
            if let Some(session) = state.sessions.get_mut(&session_id) {
                if session.pending_ask.is_some() {
                    session.status = SessionStatus::WaitingUser;
                    session.active_turn = None;
                    session.last_notice = Some("waiting for ask_user answer".into());
                    if session.usage.current_turn.is_none()
                        && let Some(usage) = usage
                    {
                        apply_usage(session, usage);
                    }
                    if session.usage.current_turn.is_none() {
                        session.usage.unavailable = true;
                    }
                    reconcile_session_timeline_scroll(session, viewport, width, is_active);
                    if is_active {
                        state.status =
                            format!("session #{session_id}: waiting for ask_user answer");
                    } else {
                        session.unread_events = session.unread_events.saturating_add(1);
                        state.status = format!("background session #{session_id} waiting user");
                    }
                    return;
                }
                session.status = SessionStatus::Idle;
                if matches!(session.stream_state, StreamState::Idle) {
                    session.stream_state = StreamState::FinalOnly;
                }
                session.active_turn = None;
                session.completed_tasks = session.completed_tasks.saturating_add(1);
                session.last_notice = Some("completed".into());
                if session.usage.current_turn.is_none()
                    && let Some(usage) = usage
                {
                    apply_usage(session, usage);
                }
                if session.usage.current_turn.is_none() {
                    session.usage.unavailable = true;
                }
                session.push_timeline(TimelineItem::System("任务完成。".into()));
                reconcile_session_timeline_scroll(session, viewport, width, is_active);
                if is_active {
                    state.status = format_active_timeline_status(session_id, "completed", session);
                } else {
                    session.unread_events = session.unread_events.saturating_add(1);
                    state.status = format!("background session #{session_id} completed");
                }
            }
        }
        TuiRuntimeEvent::Failed { session_id, error } => {
            let is_active = session_id == state.active;
            let viewport = timeline_viewport_lines(state);
            let width = timeline_content_width(state);
            if let Some(session) = state.sessions.get_mut(&session_id) {
                session.status = SessionStatus::Error;
                session.active_turn = None;
                session.failed_tasks = session.failed_tasks.saturating_add(1);
                session.last_error = Some(error.clone());
                session.last_notice = Some("failed".into());
                session.push_timeline(TimelineItem::Error(error));
                reconcile_session_timeline_scroll(session, viewport, width, is_active);
                if is_active {
                    state.status = format_active_timeline_status(session_id, "failed", session);
                } else {
                    session.unread_events = session.unread_events.saturating_add(1);
                    state.status = format!("background session #{session_id} failed");
                }
            }
        }
    }
}

fn apply_agent_event(session: &mut TuiSessionState, event: &AgentEvent, tick: u64) {
    match event {
        AgentEvent::SlashOutput { content } => {
            append_timeline_text(session, content, false);
        }
        AgentEvent::TurnStarted { turn } => {
            session.active_turn = Some(*turn);
            session.turn_started_at = Some(std::time::Instant::now());
            session.stream_state = StreamState::Idle;
            session.push_timeline(TimelineItem::System(format!("LLM Running (Turn {turn})")));
        }
        AgentEvent::AssistantMessage { content, .. } => {
            session.stream_state = StreamState::FinalOnly;
            append_timeline_text(session, content, false);
        }
        AgentEvent::AssistantMessageDelta { content, .. } => {
            session.stream_state = StreamState::Streaming;
            session.stream_metrics.content_chunks =
                session.stream_metrics.content_chunks.saturating_add(1);
            session.stream_metrics.last_delta_tick = Some(tick);
            append_timeline_text(session, content, false);
        }
        AgentEvent::ThinkingMessage { content, .. } => {
            session.thinking_state = ThinkingState::FinalOnly;
            append_timeline_text(session, content, true);
        }
        AgentEvent::ThinkingMessageDelta { content, .. } => {
            session.stream_state = StreamState::Streaming;
            session.thinking_state = ThinkingState::Streaming;
            session.stream_metrics.thinking_chunks =
                session.stream_metrics.thinking_chunks.saturating_add(1);
            session.stream_metrics.last_delta_tick = Some(tick);
            append_timeline_text(session, content, true);
        }
        AgentEvent::LlmUsage { usage, .. } => {
            session.stream_metrics.usage_chunks =
                session.stream_metrics.usage_chunks.saturating_add(1);
            apply_usage(session, usage.clone());
        }
        AgentEvent::ToolStarted {
            turn,
            index,
            name,
            args,
        } => {
            session.last_tool = Some(ToolDetail {
                turn: *turn,
                index: *index,
                name: name.clone(),
                args: args.to_string(),
                result: None,
            });
            session.push_timeline(TimelineItem::ToolCall {
                name: name.clone(),
                args: args.to_string(),
            });
        }
        AgentEvent::ToolFinished {
            turn,
            index,
            name,
            data,
        } => {
            let args = session
                .last_tool
                .as_ref()
                .filter(|tool| tool.turn == *turn && tool.index == *index && tool.name == *name)
                .map(|tool| tool.args.clone())
                .unwrap_or_default();
            session.last_tool = Some(ToolDetail {
                turn: *turn,
                index: *index,
                name: name.clone(),
                args: args.clone(),
                result: Some(data.to_string()),
            });
            if name == "ask_user"
                && let Some(pending) = pending_ask_from_tool_result(*turn, *index, data, tick)
            {
                session.pending_ask = Some(pending.clone());
                session.status = SessionStatus::WaitingUser;
                session.last_notice = Some("ask_user waiting for answer".into());
                session.push_timeline(TimelineItem::AskUser {
                    question: pending.question,
                    candidates: pending.candidates,
                });
            } else {
                session.push_timeline(TimelineItem::ToolResult {
                    name: name.clone(),
                    args,
                    data: data.to_string(),
                });
            }
        }
        AgentEvent::TurnFinished { stop_reason, .. } => {
            if let Some(start) = session.turn_started_at.take() {
                session.last_turn_elapsed = Some(start.elapsed());
            }
            session.active_turn = None;
            session.push_timeline(TimelineItem::System(format!(
                "turn finished: {stop_reason} ({:.1}s)",
                session
                    .last_turn_elapsed
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0)
            )));
        }
        AgentEvent::Stopped => {
            session.active_turn = None;
            session.pending_ask = None;
            session.push_timeline(TimelineItem::System("stopped".into()));
        }
    }
}

fn pending_ask_from_tool_result(
    turn: usize,
    index: usize,
    data: &serde_json::Value,
    tick: u64,
) -> Option<PendingAsk> {
    if data.get("status").and_then(serde_json::Value::as_str) != Some("INTERRUPT")
        || data.get("intent").and_then(serde_json::Value::as_str) != Some("HUMAN_INTERVENTION")
    {
        return None;
    }
    let payload = data.get("data")?;
    let question = payload
        .get("question")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("请提供输入：")
        .trim();
    let candidates = payload
        .get("candidates")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::trim))
                .filter(|item| !item.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(PendingAsk {
        turn,
        index,
        question: if question.is_empty() {
            "请提供输入：".into()
        } else {
            question.to_string()
        },
        candidates,
        created_tick: tick,
    })
}

fn apply_usage(session: &mut TuiSessionState, usage: koda_agent_core::LlmUsageSummary) {
    let input = usage.input_tokens.unwrap_or_default();
    let output = usage.output_tokens.unwrap_or_default();
    let total = usage_total_tokens(&usage);
    let cached = usage.cached_tokens.unwrap_or_default();
    session.usage.events = session.usage.events.saturating_add(1);
    session.usage.session_input_tokens = session.usage.session_input_tokens.saturating_add(input);
    session.usage.session_output_tokens =
        session.usage.session_output_tokens.saturating_add(output);
    session.usage.session_total_tokens = session.usage.session_total_tokens.saturating_add(total);
    session.usage.session_cached_tokens =
        session.usage.session_cached_tokens.saturating_add(cached);
    session.usage.current_turn = Some(usage);
    session.usage.unavailable = false;
}

fn append_timeline_text(session: &mut TuiSessionState, content: &str, thinking: bool) {
    if content.is_empty() {
        return;
    }
    match session.timeline.last_mut() {
        Some(TimelineItem::Assistant(text)) if !thinking && session.active_turn.is_some() => {
            text.push_str(content);
            session.mark_timeline_dirty();
            return;
        }
        Some(TimelineItem::Thinking(text)) if thinking && session.active_turn.is_some() => {
            text.push_str(content);
            session.mark_timeline_dirty();
            return;
        }
        _ => {}
    }
    if thinking {
        session.push_timeline(TimelineItem::Thinking(content.to_string()));
    } else {
        session.push_timeline(TimelineItem::Assistant(content.to_string()));
    }
}

fn reconcile_session_timeline_scroll(
    session: &mut TuiSessionState,
    viewport: usize,
    width: usize,
    is_active: bool,
) {
    trim_session_timeline(session);
    let max_scroll = max_timeline_scroll_for_width(session, viewport, width);
    if is_active && session.timeline_follow_tail {
        session.timeline_scroll = max_scroll;
        session.timeline_unseen = 0;
    } else {
        session.timeline_scroll = session.timeline_scroll.min(max_scroll);
        if is_active {
            session.timeline_unseen = session.timeline_unseen.saturating_add(1);
        }
    }
}

fn trim_session_timeline(session: &mut TuiSessionState) {
    if session.timeline.len() <= MAX_TIMELINE_ITEMS {
        return;
    }
    let remove = session.timeline.len() - MAX_TIMELINE_ITEMS;
    session.timeline.drain(0..remove);
    session.timeline_scroll = session.timeline_scroll.saturating_sub(remove as u16);
    session.mark_timeline_dirty();
}

fn format_active_timeline_status(
    session_id: usize,
    event: &str,
    session: &TuiSessionState,
) -> String {
    if session.timeline_follow_tail {
        format!("session #{session_id}: {event} | follow=on")
    } else {
        format!(
            "session #{session_id}: {event} | follow=off unseen={} | End 回到底部",
            session.timeline_unseen
        )
    }
}

fn short_event_label(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::SlashOutput { .. } => "slash output",
        AgentEvent::TurnStarted { .. } => "turn started",
        AgentEvent::AssistantMessage { .. } => "assistant",
        AgentEvent::AssistantMessageDelta { .. } => "assistant streaming",
        AgentEvent::ThinkingMessage { .. } => "thinking",
        AgentEvent::ThinkingMessageDelta { .. } => "thinking streaming",
        AgentEvent::ToolStarted { .. } => "tool started",
        AgentEvent::ToolFinished { name, .. } if name == "ask_user" => "waiting user",
        AgentEvent::ToolFinished { .. } => "tool finished",
        AgentEvent::TurnFinished { .. } => "turn finished",
        AgentEvent::LlmUsage { .. } => "usage",
        AgentEvent::Stopped => "stopped",
    }
}

fn usage_total_tokens(usage: &koda_agent_core::LlmUsageSummary) -> u64 {
    usage.total_tokens.unwrap_or_else(|| {
        usage
            .input_tokens
            .unwrap_or_default()
            .saturating_add(usage.output_tokens.unwrap_or_default())
    })
}

fn latest_usage_from_log(logs_dir: &Path) -> Option<koda_agent_core::LlmUsageSummary> {
    let text = fs::read_to_string(logs_dir.join("llm_usage.jsonl")).ok()?;
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|entry| {
            let api_mode = entry.get("api_mode")?.as_str()?.to_string();
            let model = entry.get("model")?.as_str()?.to_string();
            let usage = entry.get("usage")?;
            Some(llm_usage_summary_from_value(api_mode, model, usage))
        })
}

fn llm_usage_summary_from_value(
    api_mode: String,
    model: String,
    usage: &serde_json::Value,
) -> koda_agent_core::LlmUsageSummary {
    koda_agent_core::LlmUsageSummary {
        api_mode,
        model,
        input_tokens: usage_u64_any(
            usage,
            &[
                "/input_tokens",
                "/prompt_tokens",
                "/message/usage/input_tokens",
                "/response/usage/input_tokens",
            ],
        ),
        output_tokens: usage_u64_any(
            usage,
            &[
                "/output_tokens",
                "/completion_tokens",
                "/message/usage/output_tokens",
                "/response/usage/output_tokens",
            ],
        ),
        total_tokens: usage_u64_any(usage, &["/total_tokens", "/response/usage/total_tokens"]),
        cached_tokens: usage_u64_any(
            usage,
            &[
                "/input_tokens_details/cached_tokens",
                "/prompt_tokens_details/cached_tokens",
                "/cache_read_input_tokens",
            ],
        ),
        cache_creation_tokens: usage_u64_any(usage, &["/cache_creation_input_tokens"]),
        cache_read_tokens: usage_u64_any(usage, &["/cache_read_input_tokens"]),
        raw: usage.clone(),
    }
}

fn usage_u64_any(value: &serde_json::Value, pointers: &[&str]) -> Option<u64> {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(serde_json::Value::as_u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use std::path::Path;

    fn test_config(root: &Path) -> AgentConfig {
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
            openai_model: "mock-model".into(),
            llm_api_style: "chat".into(),
            auth_scheme: None,
            auth_header: None,
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

    fn render_to_string(width: u16, height: u16, state: &mut TuiAppState) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_app(frame, state)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut output = String::new();
        for y in 0..height {
            for x in 0..width {
                if let Some(cell) = buffer.cell((x, y)) {
                    output.push_str(cell.symbol());
                }
            }
            output.push('\n');
        }
        output
    }

    #[test]
    fn full_tui_wide_layout_renders_core_regions() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        let output = render_to_string(140, 36, &mut state);
        assert!(output.contains("Koda Agent"));
        assert!(output.contains("Sessions"));
        assert!(output.contains("Timeline"));
        assert!(output.contains("Inspector"));
        assert!(output.contains("Composer"));
        assert!(output.contains("mock-model"));
        assert_eq!(state.layout_mode, LayoutMode::Wide);
        assert_eq!(output.matches("Composer").count(), 1);
    }

    #[test]
    fn full_tui_timeline_scroll_reuses_wrapped_cache() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        {
            let session = state.active_session_mut().unwrap();
            for idx in 0..80 {
                session.push_timeline(TimelineItem::Assistant(format!(
                    "缓存滚动测试 line {idx}: {}",
                    "long中文content ".repeat(8)
                )));
            }
        }

        let _ = render_to_string(120, 24, &mut state);
        let cache_before = state
            .active_session()
            .unwrap()
            .timeline_cache
            .as_ref()
            .map(|cache| (cache.revision, cache.width, cache.signature))
            .expect("timeline cache should be built");

        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        let _ = render_to_string(120, 24, &mut state);
        let cache_after = state
            .active_session()
            .unwrap()
            .timeline_cache
            .as_ref()
            .map(|cache| (cache.revision, cache.width, cache.signature))
            .expect("timeline cache should still exist");
        assert_eq!(cache_before, cache_after);

        state
            .active_session_mut()
            .unwrap()
            .push_timeline(TimelineItem::System("cache invalidated".into()));
        let _ = render_to_string(120, 24, &mut state);
        let cache_after_append = state
            .active_session()
            .unwrap()
            .timeline_cache
            .as_ref()
            .map(|cache| cache.signature)
            .expect("timeline cache should be rebuilt");
        assert_ne!(cache_before.2, cache_after_append);
    }

    #[test]
    fn full_tui_narrow_layout_keeps_timeline_and_composer() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        let output = render_to_string(72, 28, &mut state);
        assert!(output.contains("Koda Agent"));
        assert!(output.contains("Timeline"));
        assert!(output.contains("Composer"));
        assert!(!output.contains("Inspector"));
        assert_eq!(state.layout_mode, LayoutMode::Narrow);
    }

    #[test]
    fn full_tui_timeline_renders_markdown_and_chinese_labels() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        {
            let session = state.active_session_mut().unwrap();
            session.timeline = vec![
                TimelineItem::User("请解释 `x`".into()),
                TimelineItem::Assistant("# 计划\n- 读取文件\n```rust\nfn main() {}\n```".into()),
                TimelineItem::Error("## 失败\n> 网络错误".into()),
            ];
            session.mark_timeline_dirty();
            session.timeline_follow_tail = false;
        }
        let output = render_to_string(120, 30, &mut state);
        assert!(output.contains("You"));
        assert!(output.contains("Assistant"));
        assert!(output.contains("Error"));
        assert!(output.contains("▌"));
        assert!(output.contains("•"));
        assert!(output.contains("rust"));
        assert!(output.contains("fn main"));
        assert!(output.contains("┃"));
    }

    #[test]
    fn full_tui_key_reducer_creates_sessions_and_cycles_focus() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        assert_eq!(state.active, 1);
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)
            ),
            KeyAction::NewSession
        );
        let id = state.create_session();
        assert_eq!(id, 2);
        assert_eq!(state.active, 2);
        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.focus, FocusPane::Composer);
        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            KeyAction::None
        );
        assert_eq!(state.focus, FocusPane::Timeline);
        // Ctrl+C twice to quit (2-second window)
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::None
        );
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::Quit
        );
    }

    #[test]
    fn full_tui_composer_submits_and_runtime_events_update_timeline() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(state.composer.lines().join("\n"), "hi");
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            KeyAction::Submit("hi".into())
        );
        assert!(state.composer.is_empty());

        state.active_session_mut().unwrap().status = SessionStatus::Running;
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::ToolStarted {
                    turn: 1,
                    index: 0,
                    name: "file_read".into(),
                    args: serde_json::json!({"path":"README.md"}),
                },
            },
        );
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Done {
                session_id: 1,
                usage: None,
            },
        );
        let session = state.active_session().unwrap();
        assert!(matches!(session.status, SessionStatus::Idle));
        assert!(session.active_turn.is_none());
        assert_eq!(session.last_tool.as_ref().unwrap().name, "file_read");
        assert!(session.last_tool.as_ref().unwrap().result.is_none());
        assert!(session.timeline.iter().any(
            |item| matches!(item, TimelineItem::ToolCall { name, .. } if name == "file_read")
        ));
        assert!(state.status.contains("completed"));
    }

    #[test]
    fn full_tui_streaming_deltas_render_immediately() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::TurnStarted { turn: 1 },
            },
        );
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::AssistantMessageDelta {
                    turn: 1,
                    content: "abc".into(),
                },
            },
        );
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::AssistantMessageDelta {
                    turn: 1,
                    content: "def".into(),
                },
            },
        );
        let session = state.active_session().unwrap();
        assert_eq!(session.stream_state, StreamState::Streaming);
        assert_eq!(session.stream_metrics.content_chunks, 2);
        assert!(
            matches!(session.timeline.last(), Some(TimelineItem::Assistant(text)) if text == "abcdef")
        );

        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::ThinkingMessageDelta {
                    turn: 1,
                    content: "计划".into(),
                },
            },
        );
        let session = state.active_session().unwrap();
        assert_eq!(session.thinking_state, ThinkingState::Streaming);
        assert_eq!(session.stream_metrics.thinking_chunks, 1);
        assert!(session.stream_metrics.last_delta_tick.is_some());
        assert!(
            matches!(session.timeline.last(), Some(TimelineItem::Thinking(text)) if text == "计划")
        );
        let output = render_to_string(180, 40, &mut state);
        assert!(output.contains("SSE c:2 t:1"));
    }

    #[test]
    fn full_tui_streaming_follow_tail_tracks_new_lines() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        for idx in 0..16 {
            state
                .active_session_mut()
                .unwrap()
                .timeline
                .push(TimelineItem::System(format!("line {idx}")));
        }
        let _ = render_to_string(120, 12, &mut state);
        assert!(state.active_session().unwrap().timeline_follow_tail);
        let before = state.active_session().unwrap().timeline_scroll;
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::AssistantMessageDelta {
                    turn: 1,
                    content: "第一行\n第二行\n第三行\n第四行\n第五行".into(),
                },
            },
        );
        let after = state.active_session().unwrap().timeline_scroll;
        assert!(after >= before);
        assert!(state.active_session().unwrap().timeline_follow_tail);
    }

    #[test]
    fn full_tui_completion_keeps_direct_streamed_content() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::TurnStarted { turn: 1 },
            },
        );
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::AssistantMessageDelta {
                    turn: 1,
                    content: "abcdef".into(),
                },
            },
        );
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Done {
                session_id: 1,
                usage: None,
            },
        );
        let session = state.active_session().unwrap();
        assert!(matches!(session.status, SessionStatus::Idle));
        assert!(
            session
                .timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::Assistant(text) if text == "abcdef"))
        );
        assert!(
            session
                .timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::System(text) if text == "任务完成。"))
        );
        assert!(
            !session.timeline.iter().any(
                |item| matches!(item, TimelineItem::System(text) if text.starts_with("done:"))
            )
        );
    }

    #[test]
    fn full_tui_auto_names_default_sessions_from_first_prompt() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        assert!(should_auto_name_session(state.active_session().unwrap()));
        assert_eq!(
            prompt_session_title("  帮我分析 README 和安装脚本\n给出结论  "),
            "帮我分析 README 和安装脚本 给出结论"
        );

        let session = state.active_session_mut().unwrap();
        session.name = prompt_session_title("帮我分析 README 和安装脚本\n给出结论");
        session.push_timeline(TimelineItem::User("帮我分析 README".into()));
        assert!(!should_auto_name_session(session));
    }

    #[test]
    fn full_tui_usage_tracks_current_turn_and_session_totals() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::LlmUsage {
                    turn: 1,
                    usage: koda_agent_core::LlmUsageSummary {
                        api_mode: "chat_completions".into(),
                        model: "mock-model".into(),
                        input_tokens: Some(100),
                        output_tokens: Some(20),
                        total_tokens: Some(120),
                        cached_tokens: Some(30),
                        cache_creation_tokens: None,
                        cache_read_tokens: None,
                        raw: serde_json::json!({"prompt_tokens":100}),
                    },
                },
            },
        );
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::LlmUsage {
                    turn: 2,
                    usage: koda_agent_core::LlmUsageSummary {
                        api_mode: "chat_completions".into(),
                        model: "mock-model".into(),
                        input_tokens: Some(50),
                        output_tokens: Some(10),
                        total_tokens: Some(60),
                        cached_tokens: Some(5),
                        cache_creation_tokens: None,
                        cache_read_tokens: None,
                        raw: serde_json::json!({"prompt_tokens":50}),
                    },
                },
            },
        );
        let usage = &state.active_session().unwrap().usage;
        assert_eq!(usage.current_turn.as_ref().unwrap().input_tokens, Some(50));
        assert_eq!(usage.session_input_tokens, 150);
        assert_eq!(usage.session_output_tokens, 30);
        assert_eq!(usage.session_total_tokens, 180);
        assert_eq!(usage.session_cached_tokens, 35);
        let output = render_to_string(180, 40, &mut state);
        assert!(output.contains("Token Usage"));
    }

    #[test]
    fn full_tui_tool_finish_updates_inspector_detail_and_summary() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::TurnStarted { turn: 2 },
            },
        );
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::ToolStarted {
                    turn: 2,
                    index: 1,
                    name: "web_scan".into(),
                    args: serde_json::json!({"url":"https://example.test"}),
                },
            },
        );
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::ToolFinished {
                    turn: 2,
                    index: 1,
                    name: "web_scan".into(),
                    data: serde_json::json!({"ok":true,"title":"Example"}),
                },
            },
        );
        let session = state.active_session().unwrap();
        assert_eq!(session.active_turn, Some(2));
        let tool = session.last_tool.as_ref().unwrap();
        assert_eq!(tool.name, "web_scan");
        assert!(tool.args.contains("example.test"));
        assert!(tool.result.as_ref().unwrap().contains("Example"));
        assert!(summarize_tool_result("a\n\n b").contains("a b"));

        let output = render_to_string(140, 36, &mut state);
        assert!(output.contains("Tool"));
        assert!(output.contains("web_scan"));
        assert!(output.contains("done"));
        assert!(output.contains("example.test"));
    }

    #[test]
    fn full_tui_tool_cards_adapt_common_tool_arguments() {
        let file_read = render_tool_call_card(
            "file_read",
            r#"{"path":"README.md","start":10,"count":20,"keyword":"tui","show_linenos":false}"#,
        )
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert!(file_read.contains("读取文件"));
        assert!(file_read.contains("README.md"));
        assert!(file_read.contains("keyword=tui"));
        assert!(file_read.contains("linenos=false"));

        let code_run = render_tool_call_card(
            "code_run",
            r#"{"type":"bash","code":"cargo test","timeout":120,"cwd":"crates/koda-agent-cli"}"#,
        )
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert!(code_run.contains("代码执行"));
        assert!(code_run.contains("运行 bash"));
        assert!(code_run.contains("120s"));

        let result = render_tool_result_card(
            "code_run",
            "{}",
            r#"{"status":"success","stdout":"ok\n","stderr":"","exit_code":0}"#,
            true,
        )
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert!(result.contains("exit=0"));
        assert!(result.contains("stdout=ok"));

        let expanded = render_tool_result_card(
            "file_patch",
            r#"{"path":"src/lib.rs","old_content":"old line\nnext","new_content":"new line\nnext"}"#,
            r#"{"status":"success","path":"src/lib.rs","replacements":1}"#,
            false,
        )
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert!(expanded.contains("diff preview"));
        assert!(expanded.contains("- old line"));
        assert!(expanded.contains("+ new line"));
    }

    #[test]
    fn full_tui_background_session_events_mark_unread_and_notify() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        state.create_session();
        state.active = 1;

        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 2,
                event: AgentEvent::TurnStarted { turn: 3 },
            },
        );
        let bg = state.sessions.get(&2).unwrap();
        assert_eq!(bg.unread_events, 1);
        assert_eq!(bg.active_turn, Some(3));
        assert!(state.status.contains("background session #2"));

        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Done {
                session_id: 2,
                usage: None,
            },
        );
        let bg = state.sessions.get(&2).unwrap();
        assert_eq!(bg.unread_events, 2);
        assert_eq!(bg.completed_tasks, 1);
        assert_eq!(bg.last_notice.as_deref(), Some("completed"));

        let output = render_to_string(140, 36, &mut state);
        assert!(output.contains("+2"));

        switch_session(&mut state, "2");
        assert_eq!(state.active, 2);
        assert_eq!(state.active_session().unwrap().unread_events, 0);
    }

    #[test]
    fn full_tui_active_session_events_do_not_mark_unread() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::TurnStarted { turn: 1 },
            },
        );
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Failed {
                session_id: 1,
                error: "boom".into(),
            },
        );
        let session = state.active_session().unwrap();
        assert_eq!(session.unread_events, 0);
        assert_eq!(session.failed_tasks, 1);
        assert_eq!(session.last_notice.as_deref(), Some("failed"));
        assert!(matches!(session.status, SessionStatus::Error));
    }

    #[test]
    fn full_tui_rejects_submit_when_session_running() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        state.composer = tui_textarea::TextArea::new(vec!["second task".to_string()]);
        state.active_session_mut().unwrap().status = SessionStatus::Running;
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert!(state.status.contains("already running"));
        assert_eq!(state.composer.lines().join("\n"), "second task");
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::None
        );
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::Quit
        );
    }

    #[test]
    fn full_tui_ask_user_enters_waiting_state_and_renders_choices() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::ToolStarted {
                    turn: 3,
                    index: 1,
                    name: "ask_user".into(),
                    args: serde_json::json!({
                        "question":"是否继续执行发布？",
                        "candidates":["继续","暂停","停止"]
                    }),
                },
            },
        );
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::ToolFinished {
                    turn: 3,
                    index: 1,
                    name: "ask_user".into(),
                    data: serde_json::json!({
                        "status":"INTERRUPT",
                        "intent":"HUMAN_INTERVENTION",
                        "data":{
                            "question":"是否继续执行发布？",
                            "candidates":["继续","暂停","停止"]
                        }
                    }),
                },
            },
        );

        let session = state.active_session().unwrap();
        assert!(matches!(session.status, SessionStatus::WaitingUser));
        assert_eq!(session.pending_ask.as_ref().unwrap().candidates.len(), 3);
        assert!(
            session
                .timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::AskUser { question, .. } if question.contains("继续执行发布")))
        );

        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Done {
                session_id: 1,
                usage: None,
            },
        );
        let session = state.active_session().unwrap();
        assert!(matches!(session.status, SessionStatus::WaitingUser));
        assert_eq!(session.completed_tasks, 0);

        let output = render_to_string(160, 50, &mut state);
        let compact_output = output.replace(' ', "");
        assert!(compact_output.contains("等待用户确认"));
        assert!(compact_output.contains("是否继续执行发布"));
        assert!(compact_output.contains("1.继续"));
        assert!(compact_output.contains("回答ask_user"));
    }

    #[test]
    fn full_tui_ask_user_numeric_and_text_answers_submit_same_session() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        state.active_session_mut().unwrap().pending_ask = Some(PendingAsk {
            turn: 1,
            index: 0,
            question: "下一步？".into(),
            candidates: vec!["继续".into(), "停止".into()],
            created_tick: 0,
        });
        state.active_session_mut().unwrap().status = SessionStatus::WaitingUser;

        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)
            ),
            KeyAction::Submit("停止".into())
        );
        assert!(state.active_session().unwrap().pending_ask.is_none());

        state.active_session_mut().unwrap().pending_ask = Some(PendingAsk {
            turn: 2,
            index: 0,
            question: "补充说明？".into(),
            candidates: vec![],
            created_tick: 0,
        });
        state.active_session_mut().unwrap().status = SessionStatus::WaitingUser;
        state.composer = tui_textarea::TextArea::new(vec!["/answer 使用更保守的方案".to_string()]);
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            KeyAction::Submit("使用更保守的方案".into())
        );
        assert!(state.active_session().unwrap().pending_ask.is_none());
    }

    #[test]
    fn full_tui_ask_user_cancel_clears_waiting_state() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        state.active_session_mut().unwrap().pending_ask = Some(PendingAsk {
            turn: 1,
            index: 0,
            question: "是否继续？".into(),
            candidates: vec!["继续".into()],
            created_tick: 0,
        });
        state.active_session_mut().unwrap().status = SessionStatus::WaitingUser;
        state.composer = tui_textarea::TextArea::new(vec!["/cancel".to_string()]);
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            KeyAction::Local(LocalCommand::CancelAsk)
        );
        let mut runtimes = BTreeMap::new();
        apply_local_command(&mut state, &mut runtimes, LocalCommand::CancelAsk, &cfg).unwrap();
        let session = state.active_session().unwrap();
        assert!(session.pending_ask.is_none());
        assert!(matches!(session.status, SessionStatus::Idle));
    }

    #[test]
    fn full_tui_key_reducer_keeps_text_input_from_command_keys() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert!(state.composer.is_empty());
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(state.composer.lines().join("\n"), "x");
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(state.composer.lines().join("\n"), "xq");
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(state.composer.lines().join("\n"), "x");
    }

    #[test]
    fn full_tui_reducer_ignores_release_events() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        state.focus = FocusPane::Composer;

        // A Release event for a character should not enter it into the composer.
        let release_h = KeyEvent {
            code: KeyCode::Char('h'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert_eq!(reduce_key_event(&mut state, release_h), KeyAction::None);
        assert!(state.composer.is_empty());

        // A Release of Enter should not submit empty composer.
        let release_enter = KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert_eq!(reduce_key_event(&mut state, release_enter), KeyAction::None);
        assert_eq!(state.composer.lines().join("\n"), "");

        // A Release of Backspace should not pop an already-empty composer.
        let release_bs = KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert_eq!(reduce_key_event(&mut state, release_bs), KeyAction::None);

        // Press events should still work.
        state.composer = tui_textarea::TextArea::default();
        let press_h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
        assert_eq!(reduce_key_event(&mut state, press_h), KeyAction::None);
        assert_eq!(state.composer.lines().join("\n"), "h");

        // Release of Ctrl-Q should not quit (only Press quits).
        state.composer = tui_textarea::TextArea::default();
        let release_q = KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert_eq!(reduce_key_event(&mut state, release_q), KeyAction::None);
        // Press 'q' in Composer focus appends to composer (not quit).
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        // Ctrl+C twice to quit (2-second window)
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::None
        );
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::Quit
        );
    }

    #[test]
    fn full_tui_stop_key_maps_to_abort_action() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
            ),
            KeyAction::Abort
        );
        // Esc does not quit; only Ctrl+C twice quits
        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            KeyAction::None
        );
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::None
        );
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::Quit
        );
    }

    #[test]
    fn full_tui_timeline_scroll_keys_are_bounded() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        for idx in 0..20 {
            state
                .active_session_mut()
                .unwrap()
                .timeline
                .push(TimelineItem::System(format!("line {idx}")));
        }
        let _ = render_to_string(120, 20, &mut state);
        let viewport = timeline_viewport_lines(&state);
        let width = timeline_content_width(&state);
        let max_scroll =
            max_timeline_scroll_for_width(state.active_session_mut().unwrap(), viewport, width);
        state.active_session_mut().unwrap().timeline_scroll = 0;
        state.active_session_mut().unwrap().timeline_follow_tail = false;
        assert_eq!(state.active_session().unwrap().timeline_scroll, 0);
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(
            state.active_session().unwrap().timeline_scroll,
            10.min(max_scroll)
        );
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(state.active_session().unwrap().timeline_scroll, 0);
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(state.active_session().unwrap().timeline_scroll, 0);
        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            KeyAction::None
        );
        assert_eq!(state.active_session().unwrap().timeline_scroll, 0);
        assert!(!state.active_session().unwrap().timeline_follow_tail);
        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            KeyAction::None
        );
        assert_eq!(state.active_session().unwrap().timeline_scroll, max_scroll);
        assert!(state.active_session().unwrap().timeline_follow_tail);
    }

    #[test]
    fn full_tui_timeline_auto_follows_until_user_scrolls_away() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        for idx in 0..24 {
            state
                .active_session_mut()
                .unwrap()
                .timeline
                .push(TimelineItem::System(format!("initial {idx}")));
        }
        let _ = render_to_string(120, 18, &mut state);
        let before = state.active_session().unwrap().timeline_scroll;
        assert!(before > 0);
        assert!(state.active_session().unwrap().timeline_follow_tail);

        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::AssistantMessage {
                    turn: 1,
                    content: "new output".into(),
                },
            },
        );
        assert!(state.active_session().unwrap().timeline_scroll >= before);
        assert_eq!(state.active_session().unwrap().timeline_unseen, 0);

        reduce_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 50,
                row: 8,
                modifiers: KeyModifiers::NONE,
            },
        );
        let paused_scroll = state.active_session().unwrap().timeline_scroll;
        assert!(!state.active_session().unwrap().timeline_follow_tail);
        apply_runtime_event(
            &mut state,
            TuiRuntimeEvent::Agent {
                session_id: 1,
                event: AgentEvent::AssistantMessage {
                    turn: 1,
                    content: "hidden output".into(),
                },
            },
        );
        assert_eq!(
            state.active_session().unwrap().timeline_scroll,
            paused_scroll
        );
        assert_eq!(state.active_session().unwrap().timeline_unseen, 1);
        assert!(state.status.contains("End"));

        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            KeyAction::None
        );
        assert!(state.active_session().unwrap().timeline_follow_tail);
        assert_eq!(state.active_session().unwrap().timeline_unseen, 0);
    }

    #[test]
    fn full_tui_mouse_wheel_scrolls_timeline_not_terminal_scrollback() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        for idx in 0..20 {
            state
                .active_session_mut()
                .unwrap()
                .timeline
                .push(TimelineItem::System(format!("line {idx}")));
        }
        let _ = render_to_string(120, 20, &mut state);
        state.active_session_mut().unwrap().timeline_scroll = 0;
        state.active_session_mut().unwrap().timeline_follow_tail = false;
        reduce_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(state.active_session().unwrap().timeline_scroll, 3);
        reduce_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(state.active_session().unwrap().timeline_scroll, 0);

        state.focus = FocusPane::Inspector;
        reduce_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(state.active_session().unwrap().timeline_scroll, 3);
    }

    #[test]
    fn full_tui_mouse_click_focuses_pane_before_scroll() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        let _ = render_to_string(140, 30, &mut state);

        reduce_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 124,
                row: 8,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(state.focus, FocusPane::Inspector);
        reduce_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 124,
                row: 8,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert!(state.status.contains("Inspector is fixed"));

        state.focus = FocusPane::Timeline;
        reduce_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 124,
                row: 8,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(state.focus, FocusPane::Inspector);
        assert_eq!(state.active_session().unwrap().timeline_scroll, 0);

        reduce_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 50,
                row: 8,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(state.focus, FocusPane::Timeline);
    }

    #[test]
    fn full_tui_sidebar_click_selects_session() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        state.create_session();
        state.active = 1;
        state.sessions.get_mut(&2).unwrap().unread_events = 3;
        let _ = render_to_string(140, 30, &mut state);

        reduce_mouse_event(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 2,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
        );

        assert_eq!(state.focus, FocusPane::Sessions);
        assert_eq!(state.active, 2);
        assert_eq!(state.sessions.get(&2).unwrap().unread_events, 0);
    }

    #[test]
    fn full_tui_renders_timeline_scrollbar_and_fixed_inspector() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        for idx in 0..30 {
            state
                .active_session_mut()
                .unwrap()
                .timeline
                .push(TimelineItem::System(format!("line {idx}")));
        }
        state.active_session_mut().unwrap().timeline_scroll = 4;
        state.active_session_mut().unwrap().timeline_follow_tail = false;
        let output = render_to_string(180, 40, &mut state);
        assert!(output.contains("█") || output.contains("║"));
        assert!(output.contains("scroll 4"));
    }

    #[test]
    fn full_tui_timeline_follow_tail_uses_wrapped_line_count() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        {
            let session = state.active_session_mut().unwrap();
            session.clear_timeline();
            session.push_timeline(TimelineItem::Assistant(
                "这是一个很长的中文段落，用来验证时间线按实际可视宽度换行以后，仍然可以自动跟随到最新内容。"
                    .repeat(8),
            ));
            session.push_timeline(TimelineItem::System("LATEST_MARKER".into()));
        }

        let output = render_to_string(80, 12, &mut state);
        assert!(output.contains("LATEST_MARKER"));
        assert!(state.active_session().unwrap().timeline_scroll > 0);
        assert!(state.active_session().unwrap().timeline_follow_tail);
    }

    #[test]
    fn full_tui_function_keys_offer_mac_friendly_alternatives() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            KeyAction::None
        );
        assert_eq!(state.overlay, Overlay::Help);
        state.overlay = Overlay::None;
        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)),
            KeyAction::None
        );
        assert_eq!(state.overlay, Overlay::Commands);
        state.overlay = Overlay::None;
        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE)),
            KeyAction::NewSession
        );
        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE)),
            KeyAction::Local(LocalCommand::Branch(None))
        );
        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            KeyAction::Local(LocalCommand::Clear)
        );
        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)),
            KeyAction::Local(LocalCommand::Close)
        );
    }

    #[test]
    fn full_tui_local_session_commands_parse_from_composer() {
        assert_eq!(parse_local_command("/help"), Some(LocalCommand::Help));
        assert_eq!(
            parse_local_command("/commands"),
            Some(LocalCommand::Commands)
        );
        assert_eq!(
            parse_local_command("/branch analysis"),
            Some(LocalCommand::Branch(Some("analysis".into())))
        );
        assert_eq!(
            parse_local_command("/rename main work"),
            Some(LocalCommand::Rename("main work".into()))
        );
        assert_eq!(
            parse_local_command("/switch 2"),
            Some(LocalCommand::Switch("2".into()))
        );
        assert_eq!(parse_local_command("/clear"), Some(LocalCommand::Clear));
        assert_eq!(parse_local_command("/close"), Some(LocalCommand::Close));
        assert_eq!(parse_local_command("/status"), None);

        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        state.composer = tui_textarea::TextArea::new(vec!["/rename cockpit".to_string()]);
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            KeyAction::Local(LocalCommand::Rename("cockpit".into()))
        );
        assert!(state.composer.is_empty());
    }

    #[test]
    fn full_tui_help_and_command_palette_overlay_behaviour() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);

        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(state.overlay, Overlay::Help);
        assert_eq!(
            reduce_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            KeyAction::None
        );
        assert_eq!(state.overlay, Overlay::None);

        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
            ),
            KeyAction::None
        );
        assert_eq!(state.overlay, Overlay::Commands);
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(state.overlay, Overlay::None);
        assert_eq!(state.composer.lines().join("\n"), "/rename ");

        state.composer = tui_textarea::TextArea::default();
        state.overlay = Overlay::Commands;
        // Ctrl+C twice to quit (2-second window); Ctrl+q is not a quit key
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::None
        );
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::Quit
        );
    }

    #[test]
    fn full_tui_composer_supports_multiline_input() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)
            ),
            KeyAction::None
        );
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)
            ),
            KeyAction::None
        );
        assert_eq!(state.composer.lines().join("\n"), "a\nb");
        assert_eq!(
            reduce_key_event(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            KeyAction::Submit("a\nb".into())
        );
    }

    #[test]
    fn full_tui_overlay_renders_help_and_palette() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        state.overlay = Overlay::Help;
        let help = render_to_string(140, 36, &mut state);
        assert!(help.contains("Help"));
        assert!(help.contains("Runtime Slash Commands"));

        state.overlay = Overlay::Commands;
        let commands = render_to_string(140, 36, &mut state);
        assert!(commands.contains("Command Palette"));
        assert!(commands.contains("/branch"));
        assert!(commands.contains("/continue"));
    }

    #[test]
    fn full_tui_branch_switch_rename_clear_and_close_sessions() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        let mut runtimes = BTreeMap::new();
        runtimes.insert(1, build_runtime(cfg.clone()).unwrap());

        apply_local_command(
            &mut state,
            &mut runtimes,
            LocalCommand::Branch(Some("copy".into())),
            &cfg,
        )
        .unwrap();
        assert_eq!(state.active, 2);
        assert!(runtimes.contains_key(&2));
        assert_eq!(state.active_session().unwrap().name, "copy");
        assert!(state.status.contains("branched"));

        apply_local_command(
            &mut state,
            &mut runtimes,
            LocalCommand::Rename("renamed".into()),
            &cfg,
        )
        .unwrap();
        assert_eq!(state.active_session().unwrap().name, "renamed");

        apply_local_command(
            &mut state,
            &mut runtimes,
            LocalCommand::Switch("1".into()),
            &cfg,
        )
        .unwrap();
        assert_eq!(state.active, 1);

        apply_local_command(&mut state, &mut runtimes, LocalCommand::Clear, &cfg).unwrap();
        assert_eq!(state.active_session().unwrap().timeline.len(), 1);
        assert!(matches!(
            state.active_session().unwrap().timeline.first(),
            Some(TimelineItem::System(text)) if text.contains("cleared")
        ));

        apply_local_command(
            &mut state,
            &mut runtimes,
            LocalCommand::Switch("renamed".into()),
            &cfg,
        )
        .unwrap();
        assert_eq!(state.active, 2);
        apply_local_command(&mut state, &mut runtimes, LocalCommand::Close, &cfg).unwrap();
        assert_eq!(state.active, 1);
        assert!(!state.sessions.contains_key(&2));
        assert!(!runtimes.contains_key(&2));
    }

    #[test]
    fn full_tui_rejects_closing_or_branching_running_session() {
        let d = tempfile::tempdir().unwrap();
        let cfg = test_config(d.path());
        let mut state = TuiAppState::from_config(&cfg);
        state.create_session();
        let mut runtimes = BTreeMap::new();
        runtimes.insert(1, build_runtime(cfg.clone()).unwrap());
        runtimes.insert(2, build_runtime(cfg.clone()).unwrap());
        state.active_session_mut().unwrap().status = SessionStatus::Running;

        apply_local_command(
            &mut state,
            &mut runtimes,
            LocalCommand::Branch(Some("nope".into())),
            &cfg,
        )
        .unwrap();
        assert_eq!(state.sessions.len(), 2);
        assert!(state.status.contains("cannot branch running"));

        apply_local_command(&mut state, &mut runtimes, LocalCommand::Close, &cfg).unwrap();
        assert!(state.sessions.contains_key(&2));
        assert!(state.status.contains("cannot close running"));
    }
}
