use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    response::sse::{Event, Sse},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use futures_util::{
    SinkExt, StreamExt,
    stream::{self, BoxStream},
};
use hmac::{Hmac, Mac};
use koda_agent_core::{AgentEvent, AgentRuntime};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, VecDeque},
    env,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use std::{convert::Infallible, sync::Mutex};
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use uuid::Uuid;

pub const SUPPORTED_FRONTENDS: &[&str] = &[
    "tui",
    "acp",
    "telegram",
    "feishu",
    "wecom",
    "dingtalk",
    "qq",
    "wechat",
    "desktop",
    "http",
    "tmwebdriver",
];

/// Local ACP-compatible JSONL bridge plus a legacy prompt/input JSONL fallback.
pub async fn serve_acp_jsonl(runtime: AgentRuntime) -> Result<()> {
    let factory = {
        let runtime = runtime.clone();
        Arc::new(move || Ok(runtime.clone())) as Arc<dyn Fn() -> Result<AgentRuntime> + Send + Sync>
    };
    serve_acp_jsonl_with_factory(factory).await
}

pub async fn serve_acp_jsonl_with_factory(
    runtime_factory: Arc<dyn Fn() -> Result<AgentRuntime> + Send + Sync>,
) -> Result<()> {
    let stdin = BufReader::new(io::stdin());
    let mut lines = stdin.lines();
    let stdout = Arc::new(AsyncMutex::new(io::stdout()));
    let mut sessions = BTreeMap::from([(
        "koda_default".to_string(),
        AcpSession::new(runtime_factory()?),
    )]);
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: Result<Value, _> = serde_json::from_str(line);
        let response = match parsed {
            Ok(v) if v.get("jsonrpc").and_then(Value::as_str) == Some("2.0") => {
                if v.get("method").and_then(Value::as_str) == Some("session/prompt")
                    && prompt_text_from_jsonrpc(&v).is_ok_and(|p| p == "/exit" || p == "/quit")
                {
                    break;
                }
                if v.get("method").and_then(Value::as_str) == Some("session/prompt") {
                    let response =
                        handle_jsonrpc_prompt_stream(Arc::clone(&stdout), &mut sessions, v).await?;
                    if let Some(response) = response {
                        write_jsonl(&stdout, &response).await?;
                    }
                    continue;
                }
                let (notifications, response, should_break) =
                    handle_jsonrpc(&runtime_factory, &mut sessions, v).await;
                for notification in notifications {
                    write_jsonl(&stdout, &notification).await?;
                }
                if should_break {
                    break;
                }
                response
            }
            Ok(v) => {
                let prompt = v
                    .get("prompt")
                    .or_else(|| v.get("input"))
                    .and_then(Value::as_str);
                match prompt {
                    Some("/exit") | Some("/quit") => break,
                    Some(prompt) => match sessions
                        .get("koda_default")
                        .expect("default session exists")
                        .runtime
                        .put_task(prompt.to_string())
                        .await
                    {
                        Ok(output) => json!({"ok":true,"output":output}),
                        Err(e) => json!({"ok":false,"error":format!("{e:#}")}),
                    },
                    None => json!({"ok":false,"error":"missing prompt/input"}),
                }
            }
            Err(e) => json!({"ok":false,"error":format!("invalid json: {e}")}),
        };
        write_jsonl(&stdout, &response).await?;
    }
    wait_for_acp_prompts(&sessions, Duration::from_secs(30)).await;
    Ok(())
}

#[derive(Clone)]
struct AcpSession {
    runtime: AgentRuntime,
    active_prompt_id: Arc<AsyncMutex<Option<Value>>>,
}

impl AcpSession {
    fn new(runtime: AgentRuntime) -> Self {
        Self {
            runtime,
            active_prompt_id: Arc::default(),
        }
    }
}

async fn write_jsonl(stdout: &Arc<AsyncMutex<tokio::io::Stdout>>, value: &Value) -> Result<()> {
    let mut stdout = stdout.lock().await;
    stdout
        .write_all(format!("{}\n", serde_json::to_string(value)?).as_bytes())
        .await?;
    stdout.flush().await?;
    Ok(())
}

async fn wait_for_acp_prompts(sessions: &BTreeMap<String, AcpSession>, max_wait: Duration) {
    let started = std::time::Instant::now();
    loop {
        let mut active = false;
        for session in sessions.values() {
            if session.active_prompt_id.lock().await.is_some() {
                active = true;
                break;
            }
        }
        if !active || started.elapsed() >= max_wait {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn handle_jsonrpc_prompt_stream(
    stdout: Arc<AsyncMutex<tokio::io::Stdout>>,
    sessions: &mut BTreeMap<String, AcpSession>,
    req: Value,
) -> Result<Option<Value>> {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let prompt = match prompt_text_from_jsonrpc(&req) {
        Ok(prompt) => prompt,
        Err(message) => return Ok(Some(jsonrpc_error(id, -32602, &message))),
    };
    let session_id = req
        .pointer("/params/sessionId")
        .and_then(Value::as_str)
        .unwrap_or("koda_default")
        .to_string();
    let Some(session) = sessions.get(&session_id).cloned() else {
        return Ok(Some(jsonrpc_error(id, -32602, "unknown sessionId")));
    };
    {
        let mut active = session.active_prompt_id.lock().await;
        if active.is_some() {
            return Ok(Some(jsonrpc_error(
                id,
                -32603,
                "session already has an active prompt",
            )));
        }
        *active = Some(id.clone());
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runtime = session.runtime.clone();
    let active_prompt_id = Arc::clone(&session.active_prompt_id);
    let task_session_id = session_id.clone();
    let task = tokio::spawn(async move {
        runtime
            .put_task_with_events(prompt, move |event| {
                let _ = tx.send(event);
            })
            .await
    });
    tokio::spawn(async move {
        let mut saw_turn_done = false;
        while let Some(event) = rx.recv().await {
            saw_turn_done |= matches!(event, AgentEvent::TurnFinished { .. } | AgentEvent::Stopped);
            let update = acp_update_from_event(event);
            let _ = write_jsonl(&stdout, &session_update(&task_session_id, update)).await;
        }
        let response = match task.await {
            Ok(Ok(_output)) => {
                if !saw_turn_done {
                    let done = session_update(
                        &task_session_id,
                        json!({"sessionUpdate":"agent_turn_done","stopReason":"end_turn"}),
                    );
                    let _ = write_jsonl(&stdout, &done).await;
                }
                json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn","sessionId":task_session_id}})
            }
            Ok(Err(e)) => jsonrpc_error(id.clone(), -32000, &format!("{e:#}")),
            Err(e) => jsonrpc_error(id.clone(), -32000, &format!("task join error: {e}")),
        };
        {
            let mut active = active_prompt_id.lock().await;
            if active.as_ref() == Some(&id) {
                *active = None;
            }
        }
        let _ = write_jsonl(&stdout, &response).await;
    });
    Ok(None)
}

async fn handle_jsonrpc_prompt_blocking(
    sessions: &mut BTreeMap<String, AcpSession>,
    req: Value,
) -> Result<(Vec<Value>, Value)> {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let prompt = match prompt_text_from_jsonrpc(&req) {
        Ok(prompt) => prompt,
        Err(message) => return Ok((vec![], jsonrpc_error(id, -32602, &message))),
    };
    let session_id = req
        .pointer("/params/sessionId")
        .and_then(Value::as_str)
        .unwrap_or("koda_default")
        .to_string();
    let Some(session) = sessions.get(&session_id).cloned() else {
        return Ok((vec![], jsonrpc_error(id, -32602, "unknown sessionId")));
    };
    {
        let mut active = session.active_prompt_id.lock().await;
        if active.is_some() {
            return Ok((
                vec![],
                jsonrpc_error(id, -32603, "session already has an active prompt"),
            ));
        }
        *active = Some(id.clone());
    }
    let (notifications, response) = match session.runtime.put_task(prompt).await {
        Ok(output) => {
            let notifications = vec![
                session_update(
                    &session_id,
                    json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":output}}),
                ),
                session_update(
                    &session_id,
                    json!({"sessionUpdate":"agent_turn_done","stopReason":"end_turn"}),
                ),
            ];
            (
                notifications,
                json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn","sessionId":session_id}}),
            )
        }
        Err(e) => (vec![], jsonrpc_error(id, -32000, &format!("{e:#}"))),
    };
    *session.active_prompt_id.lock().await = None;
    Ok((notifications, response))
}

fn acp_update_from_event(event: AgentEvent) -> Value {
    match event {
        AgentEvent::SlashOutput { content } => {
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":content}})
        }
        AgentEvent::TurnStarted { turn } => {
            json!({"sessionUpdate":"agent_turn_started","turn":turn})
        }
        AgentEvent::AssistantMessage { turn, content } => {
            json!({"sessionUpdate":"agent_message_chunk","turn":turn,"content":{"type":"text","text":content}})
        }
        AgentEvent::AssistantMessageDelta { turn, content } => {
            json!({"sessionUpdate":"agent_message_delta","turn":turn,"content":{"type":"text","text":content}})
        }
        AgentEvent::ThinkingMessage { turn, content } => {
            json!({"sessionUpdate":"agent_thinking","turn":turn,"content":{"type":"text","text":content}})
        }
        AgentEvent::ThinkingMessageDelta { turn, content } => {
            json!({"sessionUpdate":"agent_thinking_delta","turn":turn,"content":{"type":"text","text":content}})
        }
        AgentEvent::ToolStarted {
            turn,
            index,
            name,
            args,
        } => {
            json!({"sessionUpdate":"tool_call","turn":turn,"index":index,"name":name,"args":args})
        }
        AgentEvent::ToolFinished {
            turn,
            index,
            name,
            data,
        } => {
            json!({"sessionUpdate":"tool_result","turn":turn,"index":index,"name":name,"content":data})
        }
        AgentEvent::TurnFinished { turn, stop_reason } => {
            json!({"sessionUpdate":"agent_turn_done","turn":turn,"stopReason":stop_reason})
        }
        AgentEvent::LlmUsage { turn, usage } => {
            json!({"sessionUpdate":"agent_usage","turn":turn,"usage":usage})
        }
        AgentEvent::Stopped => {
            json!({"sessionUpdate":"agent_turn_done","stopReason":"cancelled"})
        }
    }
}

async fn handle_jsonrpc(
    runtime_factory: &Arc<dyn Fn() -> Result<AgentRuntime> + Send + Sync>,
    sessions: &mut BTreeMap<String, AcpSession>,
    req: Value,
) -> (Vec<Value>, Value, bool) {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let response = match req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "initialize" => json!({
            "jsonrpc":"2.0",
            "id":id,
            "result":{
                "protocolVersion":1,
                "agentCapabilities":{
                    "loadSession":false,
                    "mcpCapabilities":{"http":false,"sse":false},
                    "promptCapabilities":{"image":false,"audio":false,"embeddedContext":false},
                    "sessionCapabilities":{}
                },
                "agentInfo":{"name":"genericagent-acp","title":"GenericAgent","version":env!("CARGO_PKG_VERSION")},
                "authMethods":[]
            }
        }),
        "session/new" => {
            let cwd = req.pointer("/params/cwd").and_then(Value::as_str);
            if cwd.is_none_or(str::is_empty) {
                return (vec![], jsonrpc_error(id, -32602, "cwd is required"), false);
            }
            let session_id = format!("ga_{}", Uuid::new_v4().simple());
            let runtime = match runtime_factory() {
                Ok(runtime) => runtime,
                Err(e) => return (vec![], jsonrpc_error(id, -32000, &format!("{e:#}")), false),
            };
            sessions.insert(session_id.clone(), AcpSession::new(runtime));
            json!({
                "jsonrpc":"2.0",
                "id":id,
                "result":{"sessionId":session_id,"modes":Value::Null,"configOptions":Value::Null}
            })
        }
        "session/prompt" => match handle_jsonrpc_prompt_blocking(sessions, req).await {
            Ok((notifications, response)) => return (notifications, response, false),
            Err(e) => jsonrpc_error(id, -32000, &format!("{e:#}")),
        },
        "session/load" => jsonrpc_error(id, -32601, "session/load not supported"),
        "session/list" => jsonrpc_error(id, -32601, "session/list not supported"),
        "session/close" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
        "session/cancel" | "session/stop" => {
            let session_id = req
                .pointer("/params/sessionId")
                .and_then(Value::as_str)
                .unwrap_or("koda_default");
            if let Some(runtime) = sessions.get(session_id) {
                runtime.runtime.abort();
            }
            json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}})
        }
        "shutdown" => {
            return (
                vec![],
                json!({"jsonrpc":"2.0","id":id,"result":Value::Null}),
                true,
            );
        }
        "" => jsonrpc_error(id, -32600, "invalid request"),
        method => jsonrpc_error(id, -32601, &format!("method not found: {method}")),
    };
    (vec![], response, false)
}

fn session_update(session_id: &str, update: Value) -> Value {
    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session_id,"update":update}})
}

fn prompt_text_from_jsonrpc(req: &Value) -> std::result::Result<String, String> {
    let Some(prompt) = req.pointer("/params/prompt") else {
        return Err("prompt is required".to_string());
    };
    let Some(arr) = prompt.as_array() else {
        return Err("prompt must be an array".to_string());
    };
    let text = content_blocks_to_text(arr);
    if text.trim().is_empty() {
        return Err("prompt must contain text or supported content".to_string());
    }
    Ok(text)
}

fn content_blocks_to_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|block| {
            let block_type = block.get("type").and_then(Value::as_str)?;
            match block_type {
                "text" => block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                "resource_link" => {
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("resource");
                    let uri = block.get("uri").and_then(Value::as_str).unwrap_or("");
                    let desc = block
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    Some(
                        format!("[ResourceLink] {name}: {uri}\n{desc}")
                            .trim()
                            .to_string(),
                    )
                }
                "resource" => {
                    let uri = block
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or("resource");
                    let text = block.get("text").and_then(Value::as_str);
                    Some(match text {
                        Some(text) if !text.is_empty() => format!("[Resource] {uri}\n{text}"),
                        _ => format!("[Resource] {uri}"),
                    })
                }
                "image" => {
                    let uri = block
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or("inline-image");
                    Some(format!("[Image omitted] {uri}"))
                }
                other => Some(format!("[Unsupported content block: {other}]")),
            }
        })
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn jsonrpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

pub async fn run_frontend(name: &str, runtime: AgentRuntime) -> Result<()> {
    match name {
        "telegram" | "tg" => run_telegram(runtime).await,
        "http" | "web" | "desktop" => run_http_server(name, runtime).await,
        "tmwebdriver" | "tmwd" => run_tmwebdriver_master().await,
        "webhook" | "feishu" | "wecom" | "dingtalk" => run_webhook_stdin(name, runtime).await,
        "qq" | "wechat" => {
            println!(
                "Frontend '{name}' requires a native desktop/client SDK bridge. The Rust runtime is ready; use `serve-acp` or JSONL webhook bridge on this platform for now."
            );
            Ok(())
        }
        other if SUPPORTED_FRONTENDS.contains(&other) => {
            println!(
                "Frontend '{other}' is registered. Native adapter config is incomplete for this environment."
            );
            Ok(())
        }
        other => bail!("unsupported frontend: {other}"),
    }
}

#[derive(Clone)]
struct HttpState {
    runtime: AgentRuntime,
    http: Client,
}

#[derive(Debug, Deserialize, Default)]
struct IncomingMessage {
    prompt: Option<String>,
    input: Option<String>,
    text: Option<String>,
    challenge: Option<String>,
    event: Option<Value>,
    message: Option<Value>,
    #[serde(default)]
    msgtype: Option<String>,
    #[serde(default)]
    msg_type: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    system: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlatformContext {
    platform: &'static str,
    chat_id: Option<String>,
    user_id: Option<String>,
    message_id: Option<String>,
    root_id: Option<String>,
    msg_type: Option<String>,
}

async fn run_http_server(name: &str, runtime: AgentRuntime) -> Result<()> {
    let port = env::var("KODA_FRONTEND_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8787);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let state = HttpState {
        runtime,
        http: Client::new(),
    };
    let app = Router::new()
        .route("/", get(index_page))
        .route("/health", get(|| async { Json(json!({"ok":true})) }))
        .route("/status", get(http_status))
        .route("/stop", post(http_stop))
        .route("/sessions", get(http_sessions))
        .route("/message", post(http_message))
        .route("/message/stream", post(http_message_stream))
        .route("/webhook", post(http_message))
        .with_state(state);
    println!("HTTP/{name} frontend listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn http_status(State(state): State<HttpState>) -> impl IntoResponse {
    Json(json!({
        "ok":true,
        "llms": state.runtime.list_llms().into_iter().map(|(i,n,cur)| json!({"index":i,"name":n,"current":cur})).collect::<Vec<_>>()
    }))
}

async fn http_stop(State(state): State<HttpState>) -> impl IntoResponse {
    state.runtime.abort();
    Json(json!({"ok":true,"message":"stopping current task"}))
}

async fn http_sessions(State(state): State<HttpState>) -> impl IntoResponse {
    match state.runtime.put_task("/continue".to_string()).await {
        Ok(output) => Json(json!({"ok":true,"output":output})),
        Err(e) => Json(json!({"ok":false,"error":format!("{e:#}")})),
    }
}

async fn index_page() -> Html<String> {
    Html(include_str!("../resources/chat.html").to_string())
}

async fn http_message(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(body): Json<IncomingMessage>,
) -> impl IntoResponse {
    if let Some(challenge) = body.challenge {
        return Json(json!({"challenge":challenge}));
    }
    if let Err(e) = verify_callback_auth(&headers, &body) {
        return Json(json!({"ok":false,"error":e}));
    }
    let Some(prompt) = extract_prompt(&body) else {
        return Json(json!({"ok":false,"error":"missing prompt/input/text/event.message"}));
    };
    if let Some(system) = body.system.as_deref() {
        state.runtime.set_session_override("system", system);
    }
    match state.runtime.put_task(prompt).await {
        Ok(output) => {
            if let Err(e) = maybe_send_platform_reply(&state.http, &body, &output).await {
                return Json(json!({"ok":false,"output":output,"reply_error":format!("{e:#}")}));
            }
            Json(json!({"ok":true,"output":output}))
        }
        Err(e) => Json(json!({"ok":false,"error":format!("{e:#}")})),
    }
}

async fn http_message_stream(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(body): Json<IncomingMessage>,
) -> Sse<BoxStream<'static, std::result::Result<Event, Infallible>>> {
    if let Some(challenge) = body.challenge {
        return sse_once(json!({"type":"challenge","challenge":challenge}));
    }
    if let Err(e) = verify_callback_auth(&headers, &body) {
        return sse_once(json!({"type":"error","error":e}));
    }
    let Some(prompt) = extract_prompt(&body) else {
        return sse_once(json!({"type":"error","error":"missing prompt/input/text/event.message"}));
    };
    if let Some(system) = body.system.as_deref() {
        state.runtime.set_session_override("system", system);
    }
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    let runtime = state.runtime.clone();
    tokio::spawn(async move {
        let events_tx = tx.clone();
        let result = runtime
            .put_task_with_events(prompt, move |event| {
                let _ = events_tx.send(acp_update_from_event(event));
            })
            .await;
        match result {
            Ok(output) => {
                let _ = tx.send(json!({"type":"final","ok":true,"output":output}));
            }
            Err(e) => {
                let _ = tx.send(json!({"type":"error","ok":false,"error":format!("{e:#}")}));
            }
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|data| {
            (
                Ok::<_, Infallible>(Event::default().event("agent").data(data.to_string())),
                rx,
            )
        })
    })
    .boxed();
    Sse::new(stream)
}

fn sse_once(value: Value) -> Sse<BoxStream<'static, std::result::Result<Event, Infallible>>> {
    Sse::new(
        stream::once(async move { Ok(Event::default().event("agent").data(value.to_string())) })
            .boxed(),
    )
}

fn extract_prompt(body: &IncomingMessage) -> Option<String> {
    body.prompt
        .clone()
        .or_else(|| body.input.clone())
        .or_else(|| body.text.clone())
        .or_else(|| body.event.as_ref().and_then(extract_platform_text))
        .or_else(|| body.message.as_ref().and_then(extract_platform_text))
}

fn clean_platform_text(s: &str) -> String {
    serde_json::from_str::<Value>(s)
        .ok()
        .and_then(|v| extract_platform_text(&v))
        .unwrap_or_else(|| s.trim().to_string())
}

fn extract_platform_text(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(clean_platform_text(s));
    }
    if let Some(obj) = value.as_object() {
        if let Some(content) = obj.get("content").and_then(Value::as_str)
            && let Ok(parsed) = serde_json::from_str::<Value>(content)
            && let Some(text) = extract_platform_text(&parsed)
            && !text.trim().is_empty()
        {
            return Some(text);
        }
        if let Some(text_obj) = obj.get("text").and_then(Value::as_object)
            && let Some(content) = text_obj.get("content").and_then(Value::as_str)
            && !content.trim().is_empty()
        {
            return Some(content.trim().to_string());
        }
        if let Some(input) = value.pointer("/message/text").and_then(Value::as_str)
            && !input.trim().is_empty()
        {
            return Some(input.trim().to_string());
        }
        for key in [
            "text",
            "content",
            "Content",
            "msg",
            "message",
            "Message",
            "text_content",
            "raw_message",
            "plain_text",
            "Title",
        ] {
            if let Some(v) = obj.get(key)
                && let Some(text) = extract_platform_text(v)
                && !text.trim().is_empty()
            {
                return Some(text);
            }
        }
        if let Some(v) = obj.get("event").or_else(|| obj.get("Event"))
            && let Some(text) = extract_platform_text(v)
        {
            return Some(text);
        }
        if let Some(v) = obj.get("message").or_else(|| obj.get("Message"))
            && let Some(text) = extract_platform_text(v)
        {
            return Some(text);
        }
    }
    None
}

fn platform_context(body: &IncomingMessage) -> PlatformContext {
    let root = body.event.as_ref().or(body.message.as_ref());
    let platform = detect_platform(body, root);
    PlatformContext {
        platform,
        chat_id: body
            .chat_id
            .clone()
            .or_else(|| body.conversation_id.clone())
            .or_else(|| string_at(root, &["/message/chat_id", "/event/message/chat_id"]))
            .or_else(|| string_at(root, &["/conversationId", "/conversation_id", "/chatid"]))
            .or_else(|| string_at(root, &["/chat/id"]).or_else(|| string_at(root, &["/chat_id"]))),
        user_id: body
            .user_id
            .clone()
            .or_else(|| {
                string_at(
                    root,
                    &[
                        "/sender/sender_id/open_id",
                        "/event/sender/sender_id/open_id",
                    ],
                )
            })
            .or_else(|| string_at(root, &["/senderStaffId", "/senderId", "/FromUserName"]))
            .or_else(|| string_at(root, &["/from/userid", "/from/id"])),
        message_id: extract_message_id(body),
        root_id: string_at(
            root,
            &["/message/root_id", "/event/message/root_id", "/root_id"],
        )
        .or_else(|| {
            string_at(
                root,
                &[
                    "/message/parent_id",
                    "/event/message/parent_id",
                    "/parent_id",
                ],
            )
        }),
        msg_type: body
            .msgtype
            .clone()
            .or_else(|| body.msg_type.clone())
            .or_else(|| {
                string_at(
                    root,
                    &["/message/message_type", "/event/message/message_type"],
                )
            })
            .or_else(|| string_at(root, &["/msgtype", "/msg_type", "/MsgType"])),
    }
}

fn detect_platform(body: &IncomingMessage, root: Option<&Value>) -> &'static str {
    if extract_feishu_message_id(body).is_some()
        || string_at(root, &["/header/event_type"]).is_some_and(|s| s.contains("im.message"))
    {
        "feishu"
    } else if string_at(root, &["/conversationId", "/senderStaffId"]).is_some() {
        "dingtalk"
    } else if string_at(root, &["/FromUserName", "/ToUserName", "/chatid"]).is_some()
        || body.msgtype.as_deref().is_some()
    {
        "wecom"
    } else if string_at(root, &["/chat/id", "/message_id"]).is_some() {
        "telegram"
    } else {
        "generic"
    }
}

fn extract_message_id(body: &IncomingMessage) -> Option<String> {
    extract_feishu_message_id(body).or_else(|| {
        let root = body.event.as_ref().or(body.message.as_ref());
        string_at(
            root,
            &[
                "/message_id",
                "/msgid",
                "/MsgId",
                "/event/message/message_id",
                "/message/message_id",
            ],
        )
    })
}

fn string_at(root: Option<&Value>, pointers: &[&str]) -> Option<String> {
    let root = root?;
    for pointer in pointers {
        if let Some(v) = root.pointer(pointer) {
            if let Some(s) = v.as_str() {
                if !s.trim().is_empty() {
                    return Some(s.to_string());
                }
            } else if let Some(n) = v.as_i64() {
                return Some(n.to_string());
            } else if let Some(n) = v.as_u64() {
                return Some(n.to_string());
            }
        }
        if let Some(key) = pointer.strip_prefix('/')
            && !key.contains('/')
            && let Some(v) = root.get(key)
        {
            if let Some(s) = v.as_str()
                && !s.trim().is_empty()
            {
                return Some(s.to_string());
            }
            if let Some(n) = v.as_i64() {
                return Some(n.to_string());
            }
        }
    }
    None
}

async fn maybe_send_platform_reply(
    client: &Client,
    body: &IncomingMessage,
    text: &str,
) -> Result<()> {
    if let Ok(url) = env::var("FEISHU_REPLY_WEBHOOK_URL") {
        post_feishu_webhook(client, &url, text).await?;
    }
    if let Ok(url) = env::var("WECOM_WEBHOOK_URL") {
        post_wecom_webhook(client, &url, text).await?;
    }
    if let Ok(url) = env::var("DINGTALK_WEBHOOK_URL") {
        post_dingtalk_webhook(client, &url, text).await?;
    }
    let ctx = platform_context(body);
    let _attached_files = extract_file_markers(text);
    if ctx.platform == "feishu"
        && let Some(message_id) = ctx.message_id.as_deref()
        && let Some(token) = feishu_access_token(client).await?
    {
        reply_feishu_message(client, &token, message_id, text).await?;
    }
    if ctx.platform == "dingtalk"
        && let Some(conversation_id) = ctx.chat_id.as_deref()
        && let Some(token) = dingtalk_access_token(client).await?
    {
        send_dingtalk_chat_message(client, &token, conversation_id, text).await?;
    }
    Ok(())
}

fn extract_feishu_message_id(body: &IncomingMessage) -> Option<String> {
    body.event
        .as_ref()
        .and_then(|e| {
            e.pointer("/message/message_id")
                .or_else(|| e.pointer("/event/message/message_id"))
                .or_else(|| e.get("message_id"))
        })
        .or_else(|| body.message.as_ref().and_then(|m| m.get("message_id")))
        .and_then(Value::as_str)
        .map(str::to_string)
}

async fn reply_feishu_message(
    client: &Client,
    token: &str,
    message_id: &str,
    text: &str,
) -> Result<()> {
    let url = format!("https://open.feishu.cn/open-apis/im/v1/messages/{message_id}/reply");
    client
        .post(url)
        .bearer_auth(token)
        .json(&feishu_reply_payload(text))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn feishu_access_token(client: &Client) -> Result<Option<String>> {
    if let Ok(token) =
        env::var("FEISHU_BOT_ACCESS_TOKEN").or_else(|_| env::var("FEISHU_TENANT_ACCESS_TOKEN"))
    {
        return Ok(Some(token));
    }
    let (Ok(app_id), Ok(app_secret)) = (env::var("FEISHU_APP_ID"), env::var("FEISHU_APP_SECRET"))
    else {
        return Ok(None);
    };
    let value: Value = client
        .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
        .json(&json!({"app_id":app_id,"app_secret":app_secret}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(value
        .get("tenant_access_token")
        .and_then(Value::as_str)
        .map(str::to_string))
}

async fn dingtalk_access_token(client: &Client) -> Result<Option<String>> {
    if let Ok(token) = env::var("DINGTALK_ACCESS_TOKEN") {
        return Ok(Some(token));
    }
    let (Ok(app_key), Ok(app_secret)) = (
        env::var("DINGTALK_APP_KEY"),
        env::var("DINGTALK_APP_SECRET"),
    ) else {
        return Ok(None);
    };
    let value: Value = client
        .post("https://api.dingtalk.com/v1.0/oauth2/accessToken")
        .json(&json!({"appKey":app_key,"appSecret":app_secret}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(value
        .get("accessToken")
        .or_else(|| value.get("access_token"))
        .and_then(Value::as_str)
        .map(str::to_string))
}

async fn send_dingtalk_chat_message(
    client: &Client,
    token: &str,
    conversation_id: &str,
    text: &str,
) -> Result<()> {
    client
        .post("https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend")
        .bearer_auth(token)
        .json(&json!({
            "robotCode": env::var("DINGTALK_ROBOT_CODE").unwrap_or_default(),
            "userIds": [],
            "openConversationId": conversation_id,
            "msgKey": "sampleText",
            "msgParam": serde_json::to_string(&json!({"content":text})).unwrap_or_default()
        }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn post_feishu_webhook(client: &Client, url: &str, text: &str) -> Result<()> {
    client
        .post(url)
        .json(&feishu_webhook_payload(text))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn post_wecom_webhook(client: &Client, url: &str, text: &str) -> Result<()> {
    client
        .post(url)
        .json(&wecom_webhook_payload(text))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn post_dingtalk_webhook(client: &Client, url: &str, text: &str) -> Result<()> {
    client
        .post(url)
        .json(&dingtalk_webhook_payload(text))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

fn feishu_reply_payload(text: &str) -> Value {
    json!({"msg_type":"text","content":serde_json::to_string(&json!({"text":text})).unwrap_or_else(|_| "{\"text\":\"\"}".into())})
}

fn feishu_webhook_payload(text: &str) -> Value {
    json!({"msg_type":"text","content":{"text":text}})
}

fn wecom_webhook_payload(text: &str) -> Value {
    json!({"msgtype":"text","text":{"content":text}})
}

fn dingtalk_webhook_payload(text: &str) -> Value {
    json!({"msgtype":"text","text":{"content":text}})
}

fn verify_callback_auth(
    headers: &HeaderMap,
    body: &IncomingMessage,
) -> std::result::Result<(), String> {
    if let Ok(expected) = env::var("KODA_WEBHOOK_TOKEN") {
        let got = headers
            .get("x-koda-token")
            .and_then(|v| v.to_str().ok())
            .or_else(|| {
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
            });
        if got != Some(expected.as_str()) {
            return Err("invalid KODA webhook token".into());
        }
    }
    if let Ok(expected) = env::var("FEISHU_VERIFICATION_TOKEN")
        && let Some(token) = body
            .event
            .as_ref()
            .and_then(|e| e.get("token"))
            .or_else(|| body.message.as_ref().and_then(|m| m.get("token")))
            .and_then(Value::as_str)
        && token != expected
    {
        return Err("invalid Feishu verification token".into());
    }
    verify_dingtalk_signature(headers)?;
    verify_wecom_signature(headers, body)?;
    Ok(())
}

fn verify_dingtalk_signature(headers: &HeaderMap) -> std::result::Result<(), String> {
    let Ok(secret) = env::var("DINGTALK_SECRET") else {
        return Ok(());
    };
    let Some(timestamp) = headers.get("timestamp").and_then(|v| v.to_str().ok()) else {
        return Err("missing DingTalk timestamp".into());
    };
    let Some(signature) = headers.get("sign").and_then(|v| v.to_str().ok()) else {
        return Err("missing DingTalk sign".into());
    };
    let expected = dingtalk_sign(timestamp, &secret);
    if signature != expected {
        return Err("invalid DingTalk signature".into());
    }
    Ok(())
}

fn verify_wecom_signature(
    headers: &HeaderMap,
    body: &IncomingMessage,
) -> std::result::Result<(), String> {
    let Ok(token) = env::var("WECOM_TOKEN") else {
        return Ok(());
    };
    let Some(signature) = headers
        .get("msg_signature")
        .or_else(|| headers.get("x-wecom-signature"))
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(());
    };
    let timestamp = headers
        .get("timestamp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let nonce = headers
        .get("nonce")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let encrypt = body
        .event
        .as_ref()
        .and_then(|e| e.get("Encrypt"))
        .or_else(|| body.message.as_ref().and_then(|m| m.get("Encrypt")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expected = wecom_sha1_signature(&token, timestamp, nonce, encrypt);
    if signature != expected {
        return Err("invalid WeCom msg_signature".into());
    }
    Ok(())
}

fn dingtalk_sign(timestamp: &str, secret: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let string_to_sign = format!("{timestamp}\n{secret}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(string_to_sign.as_bytes());
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

fn wecom_sha1_signature(token: &str, timestamp: &str, nonce: &str, encrypt: &str) -> String {
    let mut parts = [token, timestamp, nonce, encrypt];
    parts.sort_unstable();
    let joined = parts.join("");
    let digest = Sha1::digest(joined.as_bytes());
    format!("{digest:x}")
}

pub async fn run_frontend_placeholder(name: &str) -> Result<()> {
    if SUPPORTED_FRONTENDS.contains(&name) || name == "webhook" {
        println!(
            "Frontend '{name}' is registered. Native bot SDK adapters are feature-gated and require platform credentials/config before runtime startup."
        );
        Ok(())
    } else {
        bail!("unsupported frontend: {name}")
    }
}

#[derive(Clone)]
struct TmwdSession {
    info: Value,
    transport: TmwdTransport,
    connection_id: Option<String>,
}

#[derive(Clone)]
enum TmwdTransport {
    Ws(mpsc::UnboundedSender<String>),
    Http(Arc<Mutex<VecDeque<String>>>),
}

#[derive(Clone, Default)]
struct TmwdState {
    sessions: Arc<Mutex<BTreeMap<String, TmwdSession>>>,
    pending: Arc<Mutex<BTreeMap<String, oneshot::Sender<Value>>>>,
    pending_connections: Arc<Mutex<BTreeMap<String, String>>>,
}

pub async fn run_tmwebdriver_master() -> Result<()> {
    let state = TmwdState::default();
    let ws_state = state.clone();
    let ws_task = tokio::spawn(async move { run_tmwd_ws_server(ws_state).await });
    let app = Router::new()
        .route("/", get(|| async { "tmwebdriver master ok" }))
        .route("/link", post(tmwd_http_link))
        .route("/api/longpoll", post(tmwd_http_longpoll))
        .route("/api/result", post(tmwd_http_result))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:18766").await?;
    println!(
        "TMWebDriver master listening on ws://127.0.0.1:18765 and http://127.0.0.1:18766/link"
    );
    let http_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .context("tmwebdriver http server failed")
    });
    tokio::select! {
        r = ws_task => r.context("tmwebdriver ws task join failed")??,
        r = http_task => r.context("tmwebdriver http task join failed")??,
    }
    Ok(())
}

async fn run_tmwd_ws_server(state: TmwdState) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:18765").await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tmwd_ws(stream, state).await {
                eprintln!("[tmwebdriver] ws connection error: {e:#}");
            }
        });
    }
}

async fn handle_tmwd_ws(stream: TcpStream, state: TmwdState) -> Result<()> {
    let mut probe = [0_u8; 512];
    let n = stream.peek(&mut probe).await.unwrap_or(0);
    let head = String::from_utf8_lossy(&probe[..n]).to_ascii_lowercase();
    if head.starts_with("get ") && !head.contains("upgrade: websocket") {
        let mut stream = stream;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 22\r\nconnection: close\r\n\r\ntmwebdriver master ok\n",
            )
            .await?;
        return Ok(());
    }
    let ws = accept_async(stream).await?;
    let connection_id = Uuid::new_v4().to_string();
    let (mut writer, mut reader) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let write_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if writer.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });
    while let Some(frame) = reader.next().await {
        let frame = frame?;
        let text = match frame {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            Message::Close(_) => break,
            _ => continue,
        };
        if let Ok(msg) = serde_json::from_str::<Value>(&text) {
            handle_tmwd_ws_message(&state, &tx, &connection_id, msg);
        }
    }
    cleanup_tmwd_ws_sessions(&state, &connection_id);
    write_task.abort();
    Ok(())
}

fn handle_tmwd_ws_message(
    state: &TmwdState,
    tx: &mpsc::UnboundedSender<String>,
    connection_id: &str,
    msg: Value,
) {
    match msg.get("type").and_then(Value::as_str) {
        Some("ready") => {
            if let Some(id) = msg.get("sessionId").and_then(value_as_string) {
                register_tmwd_session(state, tx, connection_id, id, msg);
            }
        }
        Some("ext_ready") | Some("tabs_update") => {
            if let Some(tabs) = msg.get("tabs").and_then(Value::as_array) {
                let current_ids = tabs
                    .iter()
                    .filter_map(|tab| tab.get("id").and_then(value_as_string))
                    .collect::<std::collections::HashSet<_>>();
                prune_tmwd_extension_sessions(state, connection_id, &current_ids);
                for tab in tabs {
                    if let Some(id) = tab.get("id").and_then(value_as_string) {
                        register_tmwd_session(state, tx, connection_id, id, tab.clone());
                    }
                }
            }
        }
        Some("result") => complete_tmwd_pending(state, &msg, true),
        Some("error") => complete_tmwd_pending(state, &msg, false),
        Some("ack") | Some("ping") | None => {}
        Some(_) => {}
    }
}

fn register_tmwd_session(
    state: &TmwdState,
    tx: &mpsc::UnboundedSender<String>,
    connection_id: &str,
    id: String,
    info: Value,
) {
    let mut info = info;
    if let Some(obj) = info.as_object_mut() {
        obj.entry("type")
            .or_insert_with(|| Value::String("ext_ws".into()));
    }
    state
        .sessions
        .lock()
        .expect("tmwebdriver session lock")
        .insert(
            id,
            TmwdSession {
                info,
                transport: TmwdTransport::Ws(tx.clone()),
                connection_id: Some(connection_id.to_string()),
            },
        );
}

fn prune_tmwd_extension_sessions(
    state: &TmwdState,
    connection_id: &str,
    current_ids: &std::collections::HashSet<String>,
) {
    state
        .sessions
        .lock()
        .expect("tmwebdriver session lock")
        .retain(|id, session| {
            let same_connection = session.connection_id.as_deref() == Some(connection_id);
            let is_ext_ws = matches!(session.transport, TmwdTransport::Ws(_))
                && session
                    .info
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("ext_ws")
                    == "ext_ws";
            !(same_connection && is_ext_ws && !current_ids.contains(id))
        });
}

fn cleanup_tmwd_ws_sessions(state: &TmwdState, connection_id: &str) {
    state
        .sessions
        .lock()
        .expect("tmwebdriver session lock")
        .retain(|_, session| {
            !matches!(session.transport, TmwdTransport::Ws(_))
                || session.connection_id.as_deref() != Some(connection_id)
        });
    let expired = {
        let mut pending_connections = state
            .pending_connections
            .lock()
            .expect("tmwebdriver pending connection lock");
        let ids = pending_connections
            .iter()
            .filter_map(|(id, conn)| (conn == connection_id).then_some(id.clone()))
            .collect::<Vec<_>>();
        for id in &ids {
            pending_connections.remove(id);
        }
        ids
    };
    let mut pending = state.pending.lock().expect("tmwebdriver pending lock");
    for id in expired {
        if let Some(tx) = pending.remove(&id) {
            let _ = tx.send(json!({"error":"tmwebdriver websocket disconnected before command completed","newTabs":[]}));
        }
    }
}

fn register_tmwd_http_session(
    state: &TmwdState,
    id: String,
    info: Value,
) -> Arc<Mutex<VecDeque<String>>> {
    let mut sessions = state.sessions.lock().expect("tmwebdriver session lock");
    if let Some(existing) = sessions.get(&id)
        && let TmwdTransport::Http(queue) = &existing.transport
    {
        return queue.clone();
    }
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    sessions.insert(
        id,
        TmwdSession {
            info,
            transport: TmwdTransport::Http(queue.clone()),
            connection_id: None,
        },
    );
    queue
}

fn complete_tmwd_pending(state: &TmwdState, msg: &Value, success: bool) {
    let Some(id) = msg.get("id").and_then(Value::as_str) else {
        return;
    };
    let tx = state
        .pending
        .lock()
        .expect("tmwebdriver pending lock")
        .remove(id);
    state
        .pending_connections
        .lock()
        .expect("tmwebdriver pending connection lock")
        .remove(id);
    if let Some(tx) = tx {
        let payload = if success {
            json!({"data":msg.get("result").cloned().unwrap_or(Value::Null),"newTabs":msg.get("newTabs").cloned().unwrap_or(json!([]))})
        } else {
            json!({"error":msg.get("error").cloned().unwrap_or(json!("unknown error")),"newTabs":msg.get("newTabs").cloned().unwrap_or(json!([]))})
        };
        let _ = tx.send(payload);
    }
}

async fn tmwd_http_link(
    State(state): State<TmwdState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let response = match body.get("cmd").and_then(Value::as_str) {
        Some("get_all_sessions") => json!({"r":tmwd_all_sessions(&state)}),
        Some("find_session") => {
            let pattern = body
                .get("url_pattern")
                .and_then(Value::as_str)
                .unwrap_or_default();
            json!({"r":tmwd_find_sessions(&state, pattern)})
        }
        Some("execute_js") => match tmwd_execute_js(&state, &body).await {
            Ok(v) => json!({"r":v}),
            Err(e) => json!({"r":{"error":format!("{e:#}")}}),
        },
        _ => json!({"ok":false,"error":"unknown tmwebdriver cmd"}),
    };
    Json(response)
}

async fn tmwd_http_longpoll(
    State(state): State<TmwdState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(session_id) = body.get("sessionId").and_then(value_as_string) else {
        return Json(json!({"id":"","ret":"missing sessionId"}));
    };
    let info = json!({
        "id": session_id,
        "url": body.get("url").cloned().unwrap_or(Value::Null),
        "title": body.get("title").cloned().unwrap_or(Value::String(String::new())),
        "type": "http",
    });
    let queue = register_tmwd_http_session(&state, session_id, info);
    for _ in 0..25 {
        if let Some(msg) = queue
            .lock()
            .expect("tmwebdriver http queue lock")
            .pop_front()
        {
            return Json(serde_json::from_str(&msg).unwrap_or_else(|_| json!({"id":"","ret":msg})));
        }
        sleep(Duration::from_millis(200)).await;
    }
    Json(json!({"id":"","ret":"next long-poll"}))
}

async fn tmwd_http_result(
    State(state): State<TmwdState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    match body.get("type").and_then(Value::as_str) {
        Some("result") => complete_tmwd_pending(&state, &body, true),
        Some("error") => complete_tmwd_pending(&state, &body, false),
        _ => {}
    }
    "ok"
}

fn tmwd_all_sessions(state: &TmwdState) -> Vec<Value> {
    state
        .sessions
        .lock()
        .expect("tmwebdriver session lock")
        .iter()
        .map(|(id, s)| {
            let mut info = s.info.clone();
            if let Some(obj) = info.as_object_mut() {
                obj.insert("id".into(), Value::String(id.clone()));
                obj.entry("type")
                    .or_insert_with(|| Value::String("ext_ws".into()));
            }
            info
        })
        .collect()
}

fn tmwd_find_sessions(state: &TmwdState, pattern: &str) -> Vec<Value> {
    tmwd_all_sessions(state)
        .into_iter()
        .filter(|v| {
            pattern.is_empty()
                || v.get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|url| url.contains(pattern))
        })
        .collect()
}

async fn tmwd_execute_js(state: &TmwdState, body: &Value) -> Result<Value> {
    let session_id = body.get("sessionId").and_then(value_as_string);
    let code = body
        .get("code")
        .cloned()
        .context("execute_js requires code")?;
    let timeout_secs = body
        .get("timeout")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| body.get("timeout").and_then(Value::as_u64))
        .unwrap_or(15);
    let result =
        tmwd_execute_once(state, session_id.as_deref(), code.clone(), timeout_secs).await?;
    if result.get("error").is_some()
        && let Some(fallback) = tmwd_cdp_runtime_fallback_code(&code)
        && let Ok(fallback_result) = tmwd_execute_once(
            state,
            session_id.as_deref(),
            Value::String(fallback.expression.clone()),
            timeout_secs,
        )
        .await
        && fallback_result.get("error").is_none()
    {
        let js_value = fallback_result.get("data").cloned().unwrap_or_else(|| {
            fallback_result
                .get("result")
                .cloned()
                .unwrap_or(Value::Null)
        });
        return Ok(json!({
            "data": {
                "result": tmwd_cdp_remote_object(js_value),
                "fallback": "tmwebdriver_plain_js",
                "fallbackCause": tmwd_error_value_to_string(result.get("error").unwrap_or(&Value::Null)),
            },
            "newTabs": fallback_result.get("newTabs").cloned().unwrap_or_else(|| json!([])),
        }));
    }
    Ok(result)
}

async fn tmwd_execute_once(
    state: &TmwdState,
    session_id: Option<&str>,
    code: Value,
    timeout_secs: u64,
) -> Result<Value> {
    let (id, session) = {
        let sessions = state.sessions.lock().expect("tmwebdriver session lock");
        session_id
            .and_then(|id| sessions.get_key_value(id))
            .or_else(|| sessions.iter().next())
            .map(|(id, session)| (id.clone(), session.clone()))
            .context("no tmwebdriver extension sessions connected")?
    };
    let exec_id = Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel();
    state
        .pending
        .lock()
        .expect("tmwebdriver pending lock")
        .insert(exec_id.clone(), tx);
    if let Some(connection_id) = &session.connection_id {
        state
            .pending_connections
            .lock()
            .expect("tmwebdriver pending connection lock")
            .insert(exec_id.clone(), connection_id.clone());
    }
    let tab_id = id
        .parse::<i64>()
        .ok()
        .map(Value::from)
        .unwrap_or(Value::String(id));
    let payload = json!({"id":exec_id,"code":code,"tabId":tab_id});
    match &session.transport {
        TmwdTransport::Ws(tx) => {
            if tx.send(payload.to_string()).is_err() {
                state
                    .pending
                    .lock()
                    .expect("tmwebdriver pending lock")
                    .remove(&exec_id);
                state
                    .pending_connections
                    .lock()
                    .expect("tmwebdriver pending connection lock")
                    .remove(&exec_id);
                bail!("tmwebdriver extension session is disconnected");
            }
        }
        TmwdTransport::Http(queue) => {
            queue
                .lock()
                .expect("tmwebdriver http queue lock")
                .push_back(payload.to_string());
        }
    }
    match timeout(Duration::from_secs(timeout_secs), rx).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(_)) => bail!("tmwebdriver response channel closed"),
        Err(_) => {
            state
                .pending
                .lock()
                .expect("tmwebdriver pending lock")
                .remove(&exec_id);
            state
                .pending_connections
                .lock()
                .expect("tmwebdriver pending connection lock")
                .remove(&exec_id);
            Ok(
                json!({"result":format!("No response data in {timeout_secs}s (script may still be running)")}),
            )
        }
    }
}

struct TmwdRuntimeFallback {
    expression: String,
}

fn tmwd_cdp_runtime_fallback_code(code: &Value) -> Option<TmwdRuntimeFallback> {
    (code.get("cmd").and_then(Value::as_str) == Some("cdp")
        && code.get("method").and_then(Value::as_str) == Some("Runtime.evaluate"))
    .then(|| {
        code.pointer("/params/expression")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
    .flatten()
    .map(|expression| TmwdRuntimeFallback { expression })
}

fn tmwd_cdp_remote_object(value: Value) -> Value {
    match value {
        Value::Null => json!({"type":"object","subtype":"null","value":Value::Null}),
        Value::Bool(v) => json!({"type":"boolean","value":v}),
        Value::Number(v) => json!({"type":"number","value":v}),
        Value::String(v) => json!({"type":"string","value":v}),
        Value::Array(v) => json!({"type":"object","subtype":"array","value":v}),
        Value::Object(v) => json!({"type":"object","value":v}),
    }
}

fn tmwd_error_value_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| value.to_string())
}

fn value_as_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|v| v.to_string()))
        .or_else(|| value.as_u64().map(|v| v.to_string()))
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: T,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    chat: TelegramChat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

async fn run_telegram(runtime: AgentRuntime) -> Result<()> {
    let token = env::var("TELEGRAM_BOT_TOKEN")
        .or_else(|_| env::var("TG_BOT_TOKEN"))
        .context("TELEGRAM_BOT_TOKEN/TG_BOT_TOKEN missing")?;
    let client = Client::new();
    let mut offset = 0_i64;
    println!("Telegram frontend started. Commands: /help /status /stop /llm /new");
    loop {
        let url = format!("https://api.telegram.org/bot{token}/getUpdates");
        let resp = client
            .get(&url)
            .query(&[("timeout", "25"), ("offset", &offset.to_string())])
            .send()
            .await?;
        let updates: TelegramResponse<Vec<TelegramUpdate>> = resp.json().await?;
        if !updates.ok {
            sleep(Duration::from_secs(3)).await;
            continue;
        }
        for update in updates.result {
            offset = update.update_id + 1;
            let Some(message) = update.message else {
                continue;
            };
            let Some(text) = message.text else { continue };
            let chat_id = message.chat.id;
            let answer = handle_chat_text(&runtime, &text).await;
            send_telegram_message(&client, &token, chat_id, &answer).await?;
        }
    }
}

async fn send_telegram_message(
    client: &Client,
    token: &str,
    chat_id: i64,
    text: &str,
) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    for chunk in split_text(text, 3500) {
        client
            .post(&url)
            .json(&json!({"chat_id":chat_id,"text":chunk}))
            .send()
            .await?
            .error_for_status()?;
    }
    Ok(())
}

async fn run_webhook_stdin(name: &str, runtime: AgentRuntime) -> Result<()> {
    let webhook = match name {
        "feishu" => env::var("FEISHU_WEBHOOK_URL").ok(),
        "wecom" => env::var("WECOM_WEBHOOK_URL").ok(),
        "dingtalk" => env::var("DINGTALK_WEBHOOK_URL").ok(),
        _ => env::var("GENERIC_WEBHOOK_URL").ok(),
    };
    println!("Webhook stdin frontend '{name}' started. Input one prompt per line.");
    let stdin = BufReader::new(io::stdin());
    let mut lines = stdin.lines();
    let client = Client::new();
    while let Some(line) = lines.next_line().await? {
        let prompt = line.trim();
        if prompt == "/quit" || prompt == "/exit" {
            break;
        }
        if prompt.is_empty() {
            continue;
        }
        let answer = handle_chat_text(&runtime, prompt).await;
        if let Some(url) = &webhook {
            post_markdown_webhook(&client, url, &answer).await?;
        } else {
            println!("{answer}");
        }
    }
    Ok(())
}

async fn post_markdown_webhook(client: &Client, url: &str, text: &str) -> Result<()> {
    let body = json!({
        "msg_type":"text",
        "content":{"text":text}
    });
    client
        .post(url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn handle_chat_text(runtime: &AgentRuntime, text: &str) -> String {
    let cmd = text.trim();
    if cmd == "/help" {
        return "Commands: /help /status /stop /llm /new. Send any other text as an agent task."
            .into();
    }
    if cmd == "/status" {
        let llm = runtime
            .list_llms()
            .into_iter()
            .map(|(i, n, cur)| format!("{} [{i}] {n}", if cur { "->" } else { "  " }))
            .collect::<Vec<_>>()
            .join("\n");
        return format!("状态: 可接收任务\nLLMs:\n{llm}");
    }
    if cmd == "/stop" {
        runtime.abort();
        return "⏹️ 正在停止当前任务".into();
    }
    match runtime.put_task(text.to_string()).await {
        Ok(out) => out,
        Err(e) => format!("❌ 错误: {e:#}"),
    }
}

fn split_text(text: &str, limit: usize) -> Vec<String> {
    if text.len() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if cur.len() + ch.len_utf8() > limit && !cur.is_empty() {
            chunks.push(cur);
            cur = String::new();
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

fn extract_file_markers(text: &str) -> Vec<String> {
    let mut files = Vec::new();
    for token in text.split_whitespace() {
        let cleaned = token.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | '[' | ']' | '(' | ')' | '<' | '>' | ',' | ';'
            )
        });
        if looks_like_local_file(cleaned) && !files.iter().any(|f| f == cleaned) {
            files.push(cleaned.to_string());
        }
    }
    files
}

fn looks_like_local_file(path: &str) -> bool {
    if path.len() < 3 {
        return false;
    }
    let has_file_ext = [
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".pdf", ".txt", ".md", ".csv", ".json", ".zip",
        ".html", ".log",
    ]
    .iter()
    .any(|ext| path.to_ascii_lowercase().ends_with(ext));
    has_file_ext && (path.starts_with('/') || path.starts_with("./") || path.starts_with("../"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use koda_agent_core::{
        AgentConfig, AgentResponse, ChatMessage, LlmClient, StepOutcome, ToolDispatcher,
    };
    use std::path::Path;
    use tempfile::tempdir;

    struct TestLlm;

    #[async_trait]
    impl LlmClient for TestLlm {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools_schema: &Value,
        ) -> Result<AgentResponse> {
            Ok(AgentResponse {
                thinking: String::new(),
                content: "ok".into(),
                tool_calls: vec![],
                raw: Value::Null,
            })
        }

        fn name(&self) -> String {
            "test-llm".into()
        }
    }

    struct TestTools;

    #[async_trait]
    impl ToolDispatcher for TestTools {
        async fn dispatch(
            &self,
            _name: &str,
            _args: Value,
            _response: &AgentResponse,
            _index: usize,
        ) -> Result<StepOutcome> {
            Ok(StepOutcome::done(json!({"status":"ok"})))
        }
    }

    fn test_cfg(root: &Path) -> AgentConfig {
        AgentConfig {
            root_dir: root.into(),
            temp_dir: root.join("temp"),
            memory_dir: root.join("memory"),
            logs_dir: root.join("logs"),
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

    fn test_runtime(root: &Path) -> AgentRuntime {
        AgentRuntime::new(test_cfg(root), Arc::new(TestLlm), Arc::new(TestTools)).unwrap()
    }

    #[test]
    fn acp_content_blocks_to_text_matches_upstream_bridge() {
        let text = content_blocks_to_text(&[
            json!({"type":"text","text":"hello"}),
            json!({"type":"resource_link","name":"Spec","uri":"file:///tmp/spec.md","description":"read this"}),
            json!({"type":"resource","uri":"file:///tmp/log.txt","text":"log body"}),
            json!({"type":"image","uri":"file:///tmp/a.png"}),
            json!({"type":"unknown_kind"}),
        ]);
        assert!(text.contains("hello"));
        assert!(text.contains("[ResourceLink] Spec: file:///tmp/spec.md\nread this"));
        assert!(text.contains("[Resource] file:///tmp/log.txt\nlog body"));
        assert!(text.contains("[Image omitted] file:///tmp/a.png"));
        assert!(text.contains("[Unsupported content block: unknown_kind]"));
        assert_eq!(
            prompt_text_from_jsonrpc(&json!({"params":{"prompt":"hello"}})).unwrap_err(),
            "prompt must be an array"
        );
        assert_eq!(
            prompt_text_from_jsonrpc(&json!({"params":{"prompt":[]}})).unwrap_err(),
            "prompt must contain text or supported content"
        );
    }

    #[tokio::test]
    async fn acp_protocol_fixtures_match_upstream_shapes() {
        let factory: Arc<dyn Fn() -> Result<AgentRuntime> + Send + Sync> =
            Arc::new(|| bail!("runtime factory should not be called"));
        let mut sessions = BTreeMap::new();
        let (_, init, stop) = handle_jsonrpc(
            &factory,
            &mut sessions,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
        )
        .await;
        assert!(!stop);
        assert_eq!(init["result"]["protocolVersion"], 1);
        assert_eq!(init["result"]["agentInfo"]["name"], "genericagent-acp");
        assert_eq!(init["result"]["authMethods"], json!([]));

        let (_, missing_cwd, _) = handle_jsonrpc(
            &factory,
            &mut sessions,
            json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}),
        )
        .await;
        assert_eq!(missing_cwd["error"]["code"], -32602);
        assert_eq!(missing_cwd["error"]["message"], "cwd is required");

        let d = tempdir().unwrap();
        let root = d.path().to_path_buf();
        let factory: Arc<dyn Fn() -> Result<AgentRuntime> + Send + Sync> =
            Arc::new(move || Ok(test_runtime(&root)));
        let (notifications, created, stop) = handle_jsonrpc(
            &factory,
            &mut sessions,
            json!({"jsonrpc":"2.0","id":22,"method":"session/new","params":{"cwd":d.path().display().to_string()}}),
        )
        .await;
        assert!(notifications.is_empty());
        assert!(!stop);
        let session_id = created["result"]["sessionId"].as_str().unwrap();
        assert!(session_id.starts_with("ga_"));
        assert!(sessions.contains_key(session_id));

        let unused_factory: Arc<dyn Fn() -> Result<AgentRuntime> + Send + Sync> =
            Arc::new(|| bail!("runtime factory should not be called"));
        let (_, unsupported, _) = handle_jsonrpc(
            &unused_factory,
            &mut sessions,
            json!({"jsonrpc":"2.0","id":3,"method":"session/list","params":{}}),
        )
        .await;
        assert_eq!(unsupported["error"]["code"], -32601);
        assert_eq!(
            unsupported["error"]["message"],
            "session/list not supported"
        );

        let (_, close, _) = handle_jsonrpc(
            &factory,
            &mut sessions,
            json!({"jsonrpc":"2.0","id":4,"method":"session/close","params":{}}),
        )
        .await;
        assert_eq!(close["result"], json!({}));

        let update = session_update(
            "ga_fixture",
            acp_update_from_event(AgentEvent::AssistantMessage {
                turn: 1,
                content: "delta".into(),
            }),
        );
        assert_eq!(update["method"], "session/update");
        assert_eq!(
            update["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(
            update["params"]["update"]["content"],
            json!({"type":"text","text":"delta"})
        );
    }

    #[tokio::test]
    async fn acp_active_prompt_and_cancel_match_upstream_shape() {
        let d = tempdir().unwrap();
        let runtime = test_runtime(d.path());
        let session = AcpSession::new(runtime.clone());
        *session.active_prompt_id.lock().await = Some(json!(10));
        let mut sessions = BTreeMap::from([("ga_busy".to_string(), session)]);
        let factory: Arc<dyn Fn() -> Result<AgentRuntime> + Send + Sync> =
            Arc::new(|| bail!("unused"));
        let (_, busy, _) = handle_jsonrpc(
            &factory,
            &mut sessions,
            json!({"jsonrpc":"2.0","id":11,"method":"session/prompt","params":{"sessionId":"ga_busy","prompt":[{"type":"text","text":"hello"}]}}),
        )
        .await;
        assert_eq!(busy["error"]["code"], -32603);
        assert_eq!(
            busy["error"]["message"],
            "session already has an active prompt"
        );

        let (_, cancel, _) = handle_jsonrpc(
            &factory,
            &mut sessions,
            json!({"jsonrpc":"2.0","id":12,"method":"session/cancel","params":{"sessionId":"ga_busy"}}),
        )
        .await;
        assert_eq!(cancel["result"]["ok"], true);
        assert!(d.path().join("temp/_stop_signal").exists());
    }

    #[test]
    fn extracts_feishu_json_text() {
        let msg = IncomingMessage {
            prompt: None,
            input: None,
            text: None,
            challenge: None,
            event: Some(json!({"message":{"content":"{\"text\":\"hello\"}"}})),
            message: None,
            ..Default::default()
        };
        assert_eq!(extract_prompt(&msg).as_deref(), Some("hello"));
    }

    #[test]
    fn extracts_nested_platform_text_shapes() {
        let feishu = IncomingMessage {
            prompt: None,
            input: None,
            text: None,
            challenge: None,
            event: Some(
                json!({"event":{"message":{"content":"{\"text\":\"/status\"}","message_id":"om_x"}}}),
            ),
            message: None,
            ..Default::default()
        };
        assert_eq!(extract_prompt(&feishu).as_deref(), Some("/status"));

        let dingtalk = IncomingMessage {
            prompt: None,
            input: None,
            text: None,
            challenge: None,
            event: Some(json!({"text":{"content":"ping"}})),
            message: None,
            ..Default::default()
        };
        assert_eq!(extract_prompt(&dingtalk).as_deref(), Some("ping"));

        let wecom = IncomingMessage {
            prompt: None,
            input: None,
            text: None,
            challenge: None,
            event: Some(json!({"Content":"hello from wecom"})),
            message: None,
            ..Default::default()
        };
        assert_eq!(extract_prompt(&wecom).as_deref(), Some("hello from wecom"));
    }

    #[test]
    fn dingtalk_signature_is_stable() {
        let sig = dingtalk_sign("1700000000000", "SECabc");
        assert!(!sig.is_empty());
        assert_eq!(sig, dingtalk_sign("1700000000000", "SECabc"));
    }

    #[test]
    fn wecom_signature_sorts_parts() {
        let a = wecom_sha1_signature("token", "1", "nonce", "encrypt");
        let b = wecom_sha1_signature("token", "1", "nonce", "encrypt");
        assert_eq!(a, b);
        assert_eq!(a.len(), 40);
    }

    #[test]
    fn outbound_payload_shapes_match_platforms() {
        assert_eq!(feishu_webhook_payload("hi")["msg_type"], "text");
        assert_eq!(wecom_webhook_payload("hi")["msgtype"], "text");
        assert_eq!(dingtalk_webhook_payload("hi")["text"]["content"], "hi");
        let payload = feishu_reply_payload("hi");
        let content = payload["content"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(content).unwrap()["text"],
            "hi"
        );
    }

    #[test]
    fn extracts_feishu_message_id() {
        let msg = IncomingMessage {
            prompt: None,
            input: None,
            text: None,
            challenge: None,
            event: Some(json!({"message":{"message_id":"om_x"}})),
            message: None,
            ..Default::default()
        };
        assert_eq!(extract_feishu_message_id(&msg).as_deref(), Some("om_x"));
    }

    #[test]
    fn platform_context_detects_common_im_shapes() {
        let feishu = IncomingMessage {
            event: Some(
                json!({"header":{"event_type":"im.message.receive_v1"},"event":{"sender":{"sender_id":{"open_id":"ou_1"}},"message":{"message_id":"om_1","root_id":"root_1","chat_id":"oc_1","message_type":"text","content":"{\"text\":\"hi\"}"}}}),
            ),
            ..Default::default()
        };
        let ctx = platform_context(&feishu);
        assert_eq!(ctx.platform, "feishu");
        assert_eq!(ctx.chat_id.as_deref(), Some("oc_1"));
        assert_eq!(ctx.user_id.as_deref(), Some("ou_1"));
        assert_eq!(ctx.message_id.as_deref(), Some("om_1"));
        assert_eq!(ctx.root_id.as_deref(), Some("root_1"));

        let dingtalk = IncomingMessage {
            event: Some(
                json!({"conversationId":"cid","senderStaffId":"uid","msgtype":"text","text":{"content":"ping"}}),
            ),
            ..Default::default()
        };
        let ctx = platform_context(&dingtalk);
        assert_eq!(ctx.platform, "dingtalk");
        assert_eq!(ctx.chat_id.as_deref(), Some("cid"));
        assert_eq!(ctx.user_id.as_deref(), Some("uid"));

        let telegram = IncomingMessage {
            message: Some(json!({"message_id":123,"chat":{"id":456},"text":"hello"})),
            ..Default::default()
        };
        let ctx = platform_context(&telegram);
        assert_eq!(ctx.platform, "telegram");
        assert_eq!(ctx.chat_id.as_deref(), Some("456"));
        assert_eq!(ctx.message_id.as_deref(), Some("123"));
    }

    #[test]
    fn extracts_more_platform_text_and_file_markers() {
        assert_eq!(
            extract_platform_text(&json!({"raw_message":"qq hello"})).as_deref(),
            Some("qq hello")
        );
        assert_eq!(
            extract_platform_text(&json!({"message":{"text":"telegram hello"}})).as_deref(),
            Some("telegram hello")
        );
        assert_eq!(
            extract_file_markers("生成文件 `/tmp/a.png` 和 ./report.pdf, 重复 /tmp/a.png"),
            vec!["/tmp/a.png".to_string(), "./report.pdf".to_string()]
        );
    }

    #[test]
    fn tmwebdriver_http_sessions_are_registered_and_listed() {
        let state = TmwdState::default();
        let queue = register_tmwd_http_session(
            &state,
            "7".into(),
            json!({"url":"https://example.com","title":"Example","type":"http"}),
        );
        queue
            .lock()
            .unwrap()
            .push_back(json!({"id":"x","code":"1+1"}).to_string());
        let sessions = tmwd_all_sessions(&state);
        assert_eq!(sessions[0]["id"], "7");
        assert_eq!(sessions[0]["type"], "http");
        assert_eq!(sessions[0]["url"], "https://example.com");
        assert_eq!(
            queue.lock().unwrap().pop_front().unwrap(),
            json!({"id":"x","code":"1+1"}).to_string()
        );
    }

    #[test]
    fn tmwebdriver_ws_disconnect_cleans_only_that_connection() {
        let state = TmwdState::default();
        let (tx_a, _rx_a) = mpsc::unbounded_channel::<String>();
        let (tx_b, _rx_b) = mpsc::unbounded_channel::<String>();
        register_tmwd_session(
            &state,
            &tx_a,
            "conn-a",
            "tab-a".into(),
            json!({"url":"https://a.example","title":"A"}),
        );
        register_tmwd_session(
            &state,
            &tx_b,
            "conn-b",
            "tab-b".into(),
            json!({"url":"https://b.example","title":"B"}),
        );
        register_tmwd_http_session(
            &state,
            "tab-http".into(),
            json!({"url":"https://h.example","title":"H","type":"http"}),
        );

        cleanup_tmwd_ws_sessions(&state, "conn-a");
        let ids = tmwd_all_sessions(&state)
            .into_iter()
            .filter_map(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
            .collect::<Vec<_>>();
        assert!(!ids.contains(&"tab-a".to_string()));
        assert!(ids.contains(&"tab-b".to_string()));
        assert!(ids.contains(&"tab-http".to_string()));
    }

    #[test]
    fn tmwebdriver_tabs_update_prunes_stale_extension_tabs() {
        let state = TmwdState::default();
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        register_tmwd_session(
            &state,
            &tx,
            "conn-a",
            "tab-old".into(),
            json!({"url":"https://old.example","title":"Old","type":"ext_ws"}),
        );
        register_tmwd_session(
            &state,
            &tx,
            "conn-a",
            "tab-current".into(),
            json!({"url":"https://current.example","title":"Current","type":"ext_ws"}),
        );
        register_tmwd_http_session(
            &state,
            "tab-http".into(),
            json!({"url":"https://h.example","title":"H","type":"http"}),
        );
        let current = ["tab-current".to_string()].into_iter().collect();

        prune_tmwd_extension_sessions(&state, "conn-a", &current);
        let ids = tmwd_all_sessions(&state)
            .into_iter()
            .filter_map(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
            .collect::<Vec<_>>();
        assert!(!ids.contains(&"tab-old".to_string()));
        assert!(ids.contains(&"tab-current".to_string()));
        assert!(ids.contains(&"tab-http".to_string()));
    }

    #[test]
    fn tmwebdriver_ws_disconnect_fails_pending_commands_immediately() {
        let state = TmwdState::default();
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        register_tmwd_session(
            &state,
            &tx,
            "conn-a",
            "tab-a".into(),
            json!({"url":"https://a.example","title":"A"}),
        );
        let (done_tx, mut done_rx) = oneshot::channel();
        state
            .pending
            .lock()
            .unwrap()
            .insert("exec-1".into(), done_tx);
        state
            .pending_connections
            .lock()
            .unwrap()
            .insert("exec-1".into(), "conn-a".into());

        cleanup_tmwd_ws_sessions(&state, "conn-a");
        let payload = done_rx.try_recv().unwrap();
        assert_eq!(
            payload["error"],
            "tmwebdriver websocket disconnected before command completed"
        );
        assert!(state.pending.lock().unwrap().is_empty());
        assert!(state.pending_connections.lock().unwrap().is_empty());
    }

    #[test]
    fn tmwebdriver_cdp_runtime_fallback_extracts_expression() {
        let code = json!({"cmd":"cdp","method":"Runtime.evaluate","params":{"expression":"document.title","returnByValue":true}});
        let fallback = tmwd_cdp_runtime_fallback_code(&code).unwrap();
        assert_eq!(fallback.expression, "document.title");
        assert_eq!(tmwd_cdp_remote_object(json!("ok"))["type"], "string");
        assert_eq!(tmwd_cdp_remote_object(json!({"a":1}))["type"], "object");
        assert!(
            tmwd_cdp_runtime_fallback_code(&json!({"cmd":"cdp","method":"Page.captureScreenshot"}))
                .is_none()
        );
    }

    #[tokio::test]
    async fn tmwebdriver_http_transport_execute_roundtrips_result() {
        let state = TmwdState::default();
        let queue = register_tmwd_http_session(
            &state,
            "9".into(),
            json!({"url":"https://example.com","title":"Example","type":"http"}),
        );
        let exec_state = state.clone();
        let handle = tokio::spawn(async move {
            tmwd_execute_js(
                &exec_state,
                &json!({"cmd":"execute_js","sessionId":"9","code":{"cmd":"management","method":"list"},"timeout":2}),
            )
            .await
        });
        let payload = loop {
            if let Some(payload) = queue.lock().unwrap().pop_front() {
                break payload;
            }
            sleep(Duration::from_millis(10)).await;
        };
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["tabId"], 9);
        assert_eq!(payload["code"]["cmd"], "management");
        complete_tmwd_pending(
            &state,
            &json!({"type":"result","id":payload["id"],"result":[{"name":"TMWD","enabled":true}],"newTabs":[]}),
            true,
        );
        let result = handle.await.unwrap().unwrap();
        assert_eq!(result["data"][0]["name"], "TMWD");
    }
}
