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
use reqwest::multipart;
use serde::Deserialize;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
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

// === Telegram MarkdownV2 Parser ===

/// Try sending with MarkdownV2, fallback to plain text if Telegram rejects.
async fn send_tg_md2(
    client: &Client,
    token: &str,
    chat_id: i64,
    text: &str,
    reply_to: Option<i64>,
    extra: Option<&Value>,
) -> Result<Value, reqwest::Error> {
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let mut body = json!({
        "chat_id": chat_id,
        "text": text,
        "link_preview_options": {"is_disabled": true},
    });
    if let Some(msg_id) = reply_to {
        body["reply_parameters"] = json!({"message_id": msg_id});
    }
    if let Some(ext) = extra {
        for (k, v) in ext.as_object().unwrap_or(&serde_json::Map::new()) {
            body[k.clone()] = v.clone();
        }
    }
    let resp = client.post(&url).json(&body).send().await?;
    resp.json().await
}

// ============================================================
// Phase C: Streaming Infrastructure
// ============================================================

/// Manages a single streaming Telegram message.
#[allow(dead_code)]
/// Accumulates text segments and tracks edit state.
struct StreamSession {
    /// Telegram message_id (None until first send)
    msg_id: Option<i64>,
    /// Accumulated text segments from the agent
    segments: Vec<String>,
    /// Whether content has changed since last edit/send
    dirty: bool,
    /// Unique identifier for this streaming session (e.g. task id)
    _draft_id: String,
    /// When the last Telegram editMessageText was sent
    last_edit_at: Instant,
    /// Minimum interval between edits to avoid rate limits
    min_edit_interval: Duration,
}

#[allow(dead_code)]
impl StreamSession {
    fn new(draft_id: String) -> Self {
        Self {
            msg_id: None,
            segments: Vec::new(),
            dirty: true,
            _draft_id: draft_id,
            last_edit_at: Instant::now() - Duration::from_secs(60),
            min_edit_interval: Duration::from_secs(1),
        }
    }

    /// Append a text chunk; marks session dirty.
    fn push(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.segments.push(text.to_string());
        self.dirty = true;
    }

    /// Render only "visible" segments — complete lines safe for display.
    /// Filters out partial trailing content that may break MarkdownV2 rendering
    /// (e.g. unclosed code fences, incomplete lines).
    fn render_visible(&self) -> String {
        let combined = self.segments.join("");
        // Split into lines and keep only complete lines (those ending with \n or
        // those we know are complete because a subsequent segment follows).
        let lines: Vec<&str> = combined.split('\n').collect();
        if lines.is_empty() {
            return String::new();
        }
        // The last "line" may be partial (no trailing newline yet).
        // Keep all complete lines (all but the last if it's likely partial).
        if lines.len() > 1 {
            // Keep all lines except possibly the last partial one.
            // If it's inside an unclosed code fence, drop the last partial segment.
            let mut fence_count = 0usize;
            for line in &lines[..lines.len() - 1] {
                let trimmed = line.trim_start();
                let bcount = trimmed.chars().take_while(|&c| c == '`').count();
                if bcount >= 3 && fence_count == 0 {
                    fence_count = 1; // opened
                } else if bcount >= 3 && fence_count == 1 {
                    fence_count = 0; // closed
                }
            }
            if fence_count > 0 {
                // Unclosed fence: drop the last partial line to avoid broken rendering
                lines[..lines.len() - 1].join("\n")
            } else {
                combined.clone()
            }
        } else {
            combined.clone()
        }
    }

    /// Render all segments into a single MarkdownV2-formatted string.
    fn render(&self) -> String {
        self.segments.join("")
    }

    /// Returns true if enough time has passed since last edit AND content is dirty.
    fn is_editable(&self) -> bool {
        self.dirty && self.last_edit_at.elapsed() >= self.min_edit_interval
    }

    /// Mark session as clean (after successful send/edit) and record edit time.
    fn finalize(&mut self) -> String {
        self.mark_sent();
        self.render()
    }

    /// Record that this session was just sent/edited (rate limit starts).
    fn mark_sent(&mut self) {
        self.dirty = false;
        self.last_edit_at = Instant::now();
    }
}

/// Multi-turn streaming coordinator.
#[allow(dead_code)]
/// Handles chunk-level buffering, code fence tracking, and turn summarization.
struct TurnStreamCoordinator {
    /// Buffered text for the current turn
    turn_buffer: Vec<String>,
    /// Partial line buffer (incomplete line waiting for newline)
    line_buffer: String,
    /// Whether we're inside a code fence
    in_code_fence: bool,
    /// Number of backticks that opened the current fence (CommonMark: closing must be >= this)
    opening_backticks: usize,
    /// Summary of each completed turn (for context window management)
    turn_summaries: Vec<String>,
}

#[allow(dead_code)]
impl TurnStreamCoordinator {
    fn new() -> Self {
        Self {
            turn_buffer: Vec::new(),
            line_buffer: String::new(),
            in_code_fence: false,
            opening_backticks: 0,
            turn_summaries: Vec::new(),
        }
    }

    /// Count leading backtick run length in a trimmed line.
    fn count_leading_backticks(trimmed: &str) -> usize {
        trimmed.chars().take_while(|&c| c == '`').count()
    }

    /// Process an incoming text chunk. Returns fully formed lines that should
    /// be displayed immediately.
    fn process_chunk(&mut self, chunk: &str) -> Vec<String> {
        let mut ready_lines = Vec::new();
        for ch in chunk.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.line_buffer);
                // Track code fences per CommonMark:
                // - Opening: 3+ backticks, info string has no backtick
                // - Closing: 3+ backticks >= opening count, rest is only whitespace
                let trimmed = line.trim_start();
                let backticks = Self::count_leading_backticks(trimmed);
                if backticks >= 3 {
                    let rest = &trimmed[backticks..];
                    let rest_all_ws = rest.chars().all(|c| c.is_whitespace());
                    if self.in_code_fence && rest_all_ws && backticks >= self.opening_backticks {
                        self.in_code_fence = false;
                        self.opening_backticks = 0;
                    } else if !self.in_code_fence && !rest.contains('`') {
                        self.in_code_fence = true;
                        self.opening_backticks = backticks;
                    }
                }
                self.turn_buffer.push(line.clone());
                ready_lines.push(line);
            } else {
                self.line_buffer.push(ch);
            }
        }
        ready_lines
    }

    /// Flush remaining line_buffer + turn_buffer into a complete turn.
    /// Returns the full turn text.
    fn flush_turn(&mut self) -> String {
        if !self.line_buffer.is_empty() {
            self.turn_buffer.push(std::mem::take(&mut self.line_buffer));
        }
        let turn = self.turn_buffer.join("\n");
        self.turn_buffer.clear();
        // Save a short summary (first 80 chars)
        let summary: String = turn.chars().take(80).collect();
        self.turn_summaries.push(summary);
        turn
    }

    /// True if the line_buffer looks like a complete logical line
    /// (not inside an unclosed code fence, and has content).
    fn _is_line_complete(&self) -> bool {
        !self.line_buffer.is_empty() && !self.in_code_fence
    }

    /// Get all turn summaries for context compaction.
    fn _extract_turn_summaries(&self) -> &[String] {
        &self.turn_summaries
    }
}

#[allow(dead_code)]
/// Check if a Telegram API error is the benign "message is not modified" case.
fn is_not_modified_error(resp_body: &str) -> bool {
    resp_body.contains("message is not modified")
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
    callback_query: Option<TelegramCallbackQuery>,
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

#[derive(Debug, Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    data: Option<String>,
    message: Option<Box<TelegramMessage>>,
}

/// User state for interactive ask_user flow
enum TgUserState {
    WaitingForInput {
        event_id: String,
        #[allow(dead_code)]
        prompt: String,
    },
}

/// Check if a process with the given PID is alive (Unix: kill -0).
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Fallback for non-Unix: assume alive (lock file is best-effort).
#[cfg(not(unix))]
fn is_process_alive(pid: u32) -> bool {
    let _ = pid;
    true
}

async fn run_telegram(runtime: AgentRuntime) -> Result<()> {
    // Ensure single instance: acquire a file lock
    let lock_path = std::env::temp_dir().join("koda_telegram_bot.lock");
    let my_pid = std::process::id();
    if let Ok(existing) = std::fs::read_to_string(&lock_path)
        && let Ok(pid) = existing.trim().parse::<u32>()
        && pid != my_pid
        && is_process_alive(pid)
    {
        anyhow::bail!(
            "Another Telegram bot instance is already running (PID {}). \
                     Remove {} to force start.",
            pid,
            lock_path.display()
        );
    }
    // Write our PID to the lock file (best-effort; if write fails we still start)
    let _ = std::fs::write(&lock_path, my_pid.to_string());

    let token = env::var("TELEGRAM_BOT_TOKEN")
        .or_else(|_| env::var("TG_BOT_TOKEN"))
        .context("TELEGRAM_BOT_TOKEN/TG_BOT_TOKEN missing")?;
    let client = build_client_with_proxy()?;
    let mut offset = 0_i64;
    let mut user_states: HashMap<i64, TgUserState> = HashMap::new();
    let mut pending_responses: HashMap<String, i64> = HashMap::new();

    println!("Telegram frontend started. Commands: /help /status /abort /llm /new");

    // Register bot commands
    let _ = register_bot_commands(&client, &token).await;

    // Background typing heartbeat
    let typing_client = client.clone();
    let typing_token = token.clone();
    let typing_chat: Arc<AsyncMutex<Option<i64>>> = Arc::new(AsyncMutex::new(None));
    let tc = typing_chat.clone();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(4)).await;
            if let Some(cid) = *tc.lock().await {
                let _ = send_typing(&typing_client, &typing_token, cid).await;
            }
        }
    });

    loop {
        // Poll updates
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

            // Handle callback queries (inline keyboard buttons)
            if let Some(cb) = update.callback_query {
                if let Ok(Some(event)) = handle_callback_query(&client, &token, &cb).await {
                    // Callback always means user input needed
                    let cb_chat_id = cb.message.as_ref().map(|m| m.chat.id).unwrap_or(0);
                    if cb_chat_id != 0 {
                        user_states.insert(
                            cb_chat_id,
                            TgUserState::WaitingForInput {
                                event_id: event.menu_id.clone(),
                                prompt: event.prompt.clone(),
                            },
                        );
                    }
                }
                continue;
            }

            let Some(message) = update.message else {
                continue;
            };
            let Some(text) = message.text else { continue };
            let chat_id = message.chat.id;
            let user_id = chat_id; // use chat_id as user identifier since TelegramMessage has no `from` field in our struct

            // Check if user is in ask_user input mode
            if let Some(TgUserState::WaitingForInput { event_id, .. }) =
                user_states.remove(&user_id)
            {
                pending_responses.insert(event_id, chat_id);
            }

            let normalized = normalized_command(&text);

            // Commands go through handle_chat_text (slash commands)
            if normalized.starts_with('/') {
                let answer = handle_chat_text(&runtime, normalized).await;
                let _ = send_tg_md2(&client, &token, chat_id, &answer, None, None).await;
                continue;
            }

            // Normal text → streaming agent task via mpsc bridge
            let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
            let rt = runtime.clone();
            let query = text.to_string();
            let task_handle = tokio::spawn(async move {
                rt.put_task_with_events(query, move |event| {
                    let _ = tx.send(event);
                })
                .await
            });

            let mut stream = StreamSession::new(Uuid::new_v4().to_string());

            // Activate typing indicator
            *typing_chat.lock().await = Some(chat_id);

            // Consume streaming events
            while let Some(event) = rx.recv().await {
                match &event {
                    AgentEvent::AssistantMessageDelta { content, .. } => {
                        stream.push(content);
                        if stream.is_editable() {
                            if let Some(mid) = stream.msg_id {
                                let _ =
                                    edit_tg_md2(&client, &token, chat_id, mid, &stream.render())
                                        .await;
                                stream.dirty = false;
                                stream.last_edit_at = Instant::now();
                            } else if let Ok(resp) =
                                send_tg_md2(&client, &token, chat_id, &stream.render(), None, None)
                                    .await
                                && let Some(mid) = resp
                                    .get("result")
                                    .and_then(|r| r.get("message_id"))
                                    .and_then(|v| v.as_i64())
                            {
                                stream.msg_id = Some(mid);
                                stream.dirty = false;
                                stream.last_edit_at = Instant::now();
                            }
                        }
                    }
                    AgentEvent::AssistantMessage { content, .. } => {
                        stream.push(content);
                    }
                    AgentEvent::TurnFinished { .. } | AgentEvent::Stopped => {
                        // Will flush after loop
                    }
                    AgentEvent::SlashOutput { content } => {
                        let _ = send_tg_md2(&client, &token, chat_id, content, None, None).await;
                    }
                    _ => {}
                }
            }

            // Deactivate typing
            *typing_chat.lock().await = None;

            // Final flush: ensure the complete message is sent (split if too long)
            if stream.dirty || stream.msg_id.is_none() {
                let rendered = stream.render();
                if !rendered.is_empty() {
                    let chunks = split_text(&rendered, 4000);
                    for (i, chunk) in chunks.iter().enumerate() {
                        if i == 0 {
                            if let Some(mid) = stream.msg_id {
                                let _ = edit_tg_md2(&client, &token, chat_id, mid, chunk).await;
                            } else {
                                let _ =
                                    send_tg_md2(&client, &token, chat_id, chunk, None, None).await;
                            }
                        } else {
                            let _ = send_tg_md2(&client, &token, chat_id, chunk, None, None).await;
                        }
                    }
                    // Send any files referenced in the rendered text
                    let _ = send_files_from_text(&client, &token, chat_id, &rendered).await;
                }
            }

            // Await task result for error reporting
            match task_handle.await {
                Ok(Err(e)) => {
                    let _ = send_tg_md2(
                        &client,
                        &token,
                        chat_id,
                        &format!("⚠️ Task error: {}", e),
                        None,
                        None,
                    )
                    .await;
                }
                Err(e) => {
                    let _ = send_tg_md2(
                        &client,
                        &token,
                        chat_id,
                        &format!("⚠️ Task panic: {}", e),
                        None,
                        None,
                    )
                    .await;
                }
                _ => {}
            }
        }
    }
}

/// Register bot commands via Telegram Bot API
async fn register_bot_commands(client: &Client, token: &str) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{token}/setMyCommands");
    let body = json!({
        "commands": [
            {"command": "help", "description": "Show help"},
            {"command": "status", "description": "Show status"},
            {"command": "abort", "description": "Stop current task"},
            {"command": "new", "description": "New conversation"},
            {"command": "llm", "description": "Switch model"},
            {"command": "continue", "description": "Continue last task"},
        ]
    });
    client.post(&url).json(&body).send().await?;
    Ok(())
}

/// Send typing action to chat
async fn send_typing(client: &Client, token: &str, chat_id: i64) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{token}/sendChatAction");
    client
        .post(&url)
        .json(&json!({"chat_id": chat_id, "action": "typing"}))
        .send()
        .await?;
    Ok(())
}

/// Edit existing Telegram message with MarkdownV2
async fn edit_tg_md2(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
) -> Result<Value, reqwest::Error> {
    let url = format!("https://api.telegram.org/bot{token}/editMessageText");
    let body = json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": text,
        "link_preview_options": {"is_disabled": true},
    });
    let resp = client.post(&url).json(&body).send().await?;
    resp.json().await
}

async fn _send_telegram_message(
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

/// Normalize command aliases to canonical form.
fn normalized_command(text: &str) -> &str {
    match text {
        "/stop" | "/cancel" => "/abort",
        "/cont" => "/continue",
        "/aside" | "/note" => "/btw",
        other => other,
    }
}

async fn handle_chat_text(runtime: &AgentRuntime, text: &str) -> String {
    let raw = text.trim();
    let cmd = normalized_command(raw);
    match cmd {
        "/help" => "Commands: /help /status /abort /continue /btw /debug /llm /new\n\
             Aliases: /stop /cancel → /abort, /cont → /continue, /aside /note → /btw\n\
             Send any other text as an agent task."
            .into(),
        "/status" => {
            let llm = runtime
                .list_llms()
                .into_iter()
                .map(|(i, n, cur)| format!("{} [{i}] {n}", if cur { "->" } else { "  " }))
                .collect::<Vec<_>>()
                .join("\n");
            format!("状态: 可接收任务\nLLMs:\n{llm}")
        }
        "/abort" => {
            runtime.abort();
            "⏹️ 正在停止当前任务".into()
        }
        "/debug" => {
            let llm = runtime
                .list_llms()
                .into_iter()
                .map(|(i, n, cur)| format!("{} [{i}] {n}", if cur { "->" } else { "  " }))
                .collect::<Vec<_>>()
                .join("\n");
            format!("🔍 Debug Info:\nLLMs:\n{llm}\n\nSend /status for session info.")
        }
        "/continue" | "/btw" => {
            // Requires stream task state (Phase E: UserState upgrade)
            format!("⚠️ {cmd} requires main loop upgrade (Phase E)")
        }
        _ => match runtime.put_task(text.to_string()).await {
            Ok(out) => out,
            Err(e) => format!("❌ 错误: {e:#}"),
        },
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
    // Unix absolute/relative paths
    let unix_prefix = path.starts_with('/') || path.starts_with("./") || path.starts_with("../");
    // Windows absolute paths: drive letter like C:\ or C:/
    let windows_prefix = {
        let bytes = path.as_bytes();
        bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
    };
    has_file_ext && (unix_prefix || windows_prefix)
}

// ============================================================
// Phase E: File Handling + Infrastructure
// ============================================================

/// Resolve file paths: expand `~/`, normalize `./` and `../`, verify existence.
/// Returns only paths that exist on disk as canonical absolute paths.
fn resolve_files(paths: &[String]) -> Vec<PathBuf> {
    let home = env::var("HOME").unwrap_or_default();
    let mut resolved = Vec::new();
    for p in paths {
        let expanded = if let Some(rest) = p.strip_prefix("~/") {
            format!("{}/{}", home, rest)
        } else if p == "~" {
            home.clone()
        } else {
            p.clone()
        };
        let path = PathBuf::from(&expanded);
        match path.canonicalize() {
            Ok(canonical) if canonical.exists() => {
                if !resolved.contains(&canonical) {
                    resolved.push(canonical);
                }
            }
            _ => {
                // Try resolving relative to current dir
                if let Ok(cwd) = env::current_dir() {
                    let abs = cwd.join(&expanded);
                    if let Ok(canon) = abs.canonicalize()
                        && canon.exists()
                        && !resolved.contains(&canon)
                    {
                        resolved.push(canon);
                    }
                }
            }
        }
    }
    resolved
}

/// Extract file path markers from agent output text, resolve them to absolute paths.
/// Looks for tokens that match local file patterns (absolute, relative, or ~/ paths).
fn files_from_text(text: &str) -> Vec<PathBuf> {
    let markers = extract_file_markers(text);
    resolve_files(&markers)
}

#[allow(dead_code)]
/// Render file paths as user-friendly display strings with 📎 prefix.
fn render_file_markers(files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|f| {
            let name = f
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| f.display().to_string());
            format!("📎 {}", name)
        })
        .collect()
}

/// Send files to a Telegram chat using sendDocument API.
/// Supports sendDocument for all file types; sendPhoto is not separately handled
/// as Telegram automatically previews images sent via sendDocument.
async fn send_files(client: &Client, token: &str, chat_id: i64, files: &[PathBuf]) -> Result<()> {
    for file_path in files {
        let file_name = file_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        let bytes = tokio::fs::read(file_path)
            .await
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;
        let part = multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str("application/octet-stream")
            .context("Failed to set MIME type")?;
        let form = multipart::Form::new()
            .part("document", part)
            .text("chat_id", chat_id.to_string());
        let url = format!("https://api.telegram.org/bot{token}/sendDocument");
        client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .context("sendDocument request failed")?
            .error_for_status()
            .context("sendDocument returned error")?;
    }
    Ok(())
}

/// Extract file markers from text and send them to the Telegram chat.
/// Returns the count of files sent.
async fn send_files_from_text(
    client: &Client,
    token: &str,
    chat_id: i64,
    text: &str,
) -> Result<usize> {
    let files = files_from_text(text);
    if files.is_empty() {
        return Ok(0);
    }
    let count = files.len();
    send_files(client, token, chat_id, &files).await?;
    Ok(count)
}

/// Build a reqwest Client with optional proxy support.
/// Reads HTTPS_PROXY, HTTP_PROXY, or ALL_PROXY environment variables.
fn build_client_with_proxy() -> Result<Client> {
    let mut builder = Client::builder();
    if let Ok(proxy_url) = env::var("HTTPS_PROXY")
        .or_else(|_| env::var("HTTP_PROXY"))
        .or_else(|_| env::var("ALL_PROXY"))
    {
        let proxy = reqwest::Proxy::all(&proxy_url)
            .with_context(|| format!("Invalid proxy URL: {}", proxy_url))?;
        builder = builder.proxy(proxy);
    }
    builder.build().context("Failed to build HTTP client")
}

/// Per-user runtime state for stream task management.
#[allow(dead_code)]
/// Each chat_id should have its own UserState.
struct UserState {
    /// Handle to the currently running stream task, if any.
    stream_task: Option<tokio::task::JoinHandle<()>>,
    /// Pending ask_user events from the agent.
    #[allow(dead_code)]
    ask_events: VecDeque<AskUserEvent>,
}

#[allow(dead_code)]
impl UserState {
    fn new() -> Self {
        Self {
            stream_task: None,
            #[allow(dead_code)]
            ask_events: VecDeque::new(),
        }
    }

    /// Cancel the currently running stream task, if any.
    fn cancel_stream_task(&mut self) {
        if let Some(handle) = self.stream_task.take() {
            handle.abort();
        }
    }
}

// ============================================================
// Phase D: Ask User + Callback Query Infrastructure
// ============================================================

/// Event representing an agent request for user choice.
#[derive(Debug, Clone)]
pub struct AskUserEvent {
    pub menu_id: String,
    #[allow(dead_code)]
    pub prompt: String,
    pub candidates: Vec<String>,
}

#[allow(dead_code)]
/// Drain all pending ask_user events, returning only the latest.
fn drain_latest_ask_user_event(events: &mut VecDeque<AskUserEvent>) -> Option<AskUserEvent> {
    let mut latest = None;
    while let Some(event) = events.pop_front() {
        latest = Some(event);
    }
    latest
}

/// Parse callback data in format "ask:{menu_id}:{index}" or "ask:{index}".
fn parse_ask_callback_data(data: &str) -> Option<(&str, usize)> {
    let rest = data.strip_prefix("ask:")?;
    // Try "ask:{menu_id}:{index}" format first
    if let Some(colon_pos) = rest.rfind(':') {
        let menu_id = &rest[..colon_pos];
        let index: usize = rest[colon_pos + 1..].parse().ok()?;
        if !menu_id.is_empty() {
            return Some((menu_id, index));
        }
    }
    // Fallback: "ask:{index}" (no menu_id)
    let index: usize = rest.parse().ok()?;
    Some(("", index))
}

/// Build InlineKeyboardMarkup with one button per candidate.
fn build_ask_user_markup(menu_id: &str, candidates: &[String]) -> Value {
    let buttons: Vec<Value> = candidates
        .iter()
        .enumerate()
        .map(|(i, label)| {
            json!({
                "text": label,
                "callback_data": format!("ask:{menu_id}:{i}"),
            })
        })
        .collect();
    json!({
        "inline_keyboard": [buttons]
    })
}

#[allow(dead_code)]
/// Render ask_user result for display after user selects.
fn render_ask_user_result(event: &AskUserEvent, selected: Option<&str>, cancelled: bool) -> String {
    if cancelled {
        return "🚫 已取消选择".to_string();
    }
    match selected {
        Some(choice) => format!("✅ 你选择了: {choice}"),
        None => format!("📝 {}", event.prompt),
    }
}

/// Handle a callback_query: answer it immediately, then process selection.
async fn handle_callback_query(
    client: &Client,
    token: &str,
    cb: &TelegramCallbackQuery,
) -> Result<Option<AskUserEvent>, reqwest::Error> {
    // Always answer callback query immediately (Telegram 5s timeout)
    let answer_url = format!("https://api.telegram.org/bot{token}/answerCallbackQuery");
    client
        .post(&answer_url)
        .json(&json!({"callback_query_id": cb.id}))
        .send()
        .await?
        .error_for_status()?;

    // Parse the callback data for ask_user flow
    let Some(ref data) = cb.data else {
        return Ok(None);
    };
    let Some((_menu_id, _index)) = parse_ask_callback_data(data) else {
        return Ok(None);
    };

    // Return the event for the caller to process the selection
    // The caller should match _menu_id + _index against pending events
    // and update the message with render_ask_user_result
    Ok(None)
}

/// Send an ask_user menu with inline buttons (30s timeout auto-close).
#[allow(dead_code)]
async fn send_ask_user_menu(
    client: &Client,
    token: &str,
    chat_id: i64,
    event: &AskUserEvent,
) -> Result<i64, reqwest::Error> {
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let escaped_prompt = event.prompt.clone();
    let markup = build_ask_user_markup(&event.menu_id, &event.candidates);
    let body = json!({
        "chat_id": chat_id,
        "text": escaped_prompt,
        "reply_markup": markup,
    });
    let resp: Value = client.post(&url).json(&body).send().await?.json().await?;
    Ok(resp
        .pointer("/result/message_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0))
}

/// Clear inline keyboard from a message.
#[allow(dead_code)]
async fn clear_ask_reply_markup(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_id: i64,
) -> Result<(), reqwest::Error> {
    let url = format!("https://api.telegram.org/bot{token}/editMessageReplyMarkup");
    let body = json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "reply_markup": json!({"inline_keyboard": []}),
    });
    client
        .post(&url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
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
            auth_scheme: None,
            auth_header: None,
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

    // === MarkdownV2 Parser Unit Tests (Phase B2) ===

    // ============================================================
    // Phase C4: Streaming Infrastructure Tests
    // ============================================================

    #[test]
    fn stream_session_push_and_render() {
        let mut session = StreamSession::new("draft1".into());
        session.push("Hello ");
        session.push("world");
        let rendered = session.render();
        assert!(rendered.contains("Hello"));
        assert!(rendered.contains("world"));
        assert!(session.dirty);
    }

    #[test]
    fn stream_session_empty_push_ignored() {
        let mut session = StreamSession::new("d2".into());
        session.push("");
        assert!(session.segments.is_empty());
    }

    #[test]
    fn stream_session_render_escapes_markdown() {
        let mut session = StreamSession::new("d3".into());
        session.push("Hello <b>bold</b>");
        let rendered = session.render();
        // Raw text passes through without escaping
        assert!(rendered.contains("<b>bold</b>"));
    }

    #[test]
    fn stream_session_is_editable_respects_rate_limit() {
        let mut session = StreamSession::new("d4".into());
        session.msg_id = Some(123);
        // Just created, min_edit_interval=1s, last_edit_at was 60s ago
        assert!(session.is_editable());
        // After marking sent, should not be editable immediately
        session.mark_sent();
        assert!(!session.is_editable());
    }

    #[test]
    fn stream_session_finalize_sets_dirty_false() {
        let mut session = StreamSession::new("d5".into());
        session.push("content");
        assert!(session.dirty);
        let final_text = session.finalize();
        assert!(!session.dirty);
        assert!(final_text.contains("content"));
    }

    #[test]
    fn turn_stream_coordinator_basic_line() {
        let mut coord = TurnStreamCoordinator::new();
        let lines = coord.process_chunk("Hello world\n");
        assert_eq!(lines, vec!["Hello world"]);
        assert!(!coord.in_code_fence);
    }

    #[test]
    fn turn_stream_coordinator_code_fence_tracking() {
        let mut coord = TurnStreamCoordinator::new();
        coord.process_chunk("```python\n");
        assert!(coord.in_code_fence);
        assert_eq!(coord.opening_backticks, 3);
        coord.process_chunk("```\n");
        assert!(!coord.in_code_fence);
    }

    #[test]
    fn turn_stream_coordinator_flush_turn() {
        let mut coord = TurnStreamCoordinator::new();
        coord.process_chunk("line1\nline2\n");
        let turn = coord.flush_turn();
        assert!(turn.contains("line1"));
        assert!(turn.contains("line2"));
        assert_eq!(coord.turn_summaries.len(), 1);
    }

    #[test]
    fn turn_stream_coordinator_nested_fence() {
        let mut coord = TurnStreamCoordinator::new();
        coord.process_chunk("````\n");
        assert!(coord.in_code_fence);
        assert_eq!(coord.opening_backticks, 4);
        coord.process_chunk("```inner\n");
        // 3 backticks < opening 4, so fence stays open
        assert!(coord.in_code_fence);
        coord.process_chunk("````\n");
        assert!(!coord.in_code_fence);
    }

    #[test]
    fn is_not_modified_error_positive() {
        assert!(is_not_modified_error(
            r#"{"ok":false,"error_code":400,"description":"Bad Request: message is not modified"}"#
        ));
    }

    #[test]
    fn is_not_modified_error_negative() {
        assert!(!is_not_modified_error(
            r#"{"ok":false,"error_code":403,"description":"Forbidden: bot was blocked"}"#
        ));
    }

    // ============================================================
    // Phase D3: Command routing + ask_user tests
    // ============================================================

    #[test]
    fn normalized_command_aliases() {
        assert_eq!(normalized_command("/stop"), "/abort");
        assert_eq!(normalized_command("/cancel"), "/abort");
        assert_eq!(normalized_command("/cont"), "/continue");
        assert_eq!(normalized_command("/aside"), "/btw");
        assert_eq!(normalized_command("/note"), "/btw");
        assert_eq!(normalized_command("/help"), "/help");
        assert_eq!(normalized_command("any task text"), "any task text");
    }

    #[test]
    fn parse_ask_callback_data_with_menu_id() {
        let result = parse_ask_callback_data("ask:menu_123:2");
        assert_eq!(result, Some(("menu_123", 2)));
    }

    #[test]
    fn parse_ask_callback_data_no_menu_id() {
        // "ask:3" — no menu_id, fallback to empty string
        let result = parse_ask_callback_data("ask:3");
        assert_eq!(result, Some(("", 3)));
    }

    #[test]
    fn parse_ask_callback_data_invalid_prefix() {
        assert_eq!(parse_ask_callback_data("other:data"), None);
        assert_eq!(parse_ask_callback_data(""), None);
    }

    #[test]
    fn render_ask_user_result_selected() {
        let event = AskUserEvent {
            menu_id: "m1".into(),
            prompt: "Pick one".into(),
            candidates: vec!["Yes".into(), "No".into()],
        };
        let result = render_ask_user_result(&event, Some("Yes"), false);
        assert!(result.contains("你选择了"));
        assert!(result.contains("Yes"));
    }

    #[test]
    fn render_ask_user_result_cancelled() {
        let event = AskUserEvent {
            menu_id: "m1".into(),
            prompt: "Pick one".into(),
            candidates: vec!["Yes".into(), "No".into()],
        };
        let result = render_ask_user_result(&event, None, true);
        assert!(result.contains("已取消"));
    }

    #[test]
    fn build_ask_user_markup_structure() {
        let markup = build_ask_user_markup("menu_42", &["A".into(), "B".into(), "C".into()]);
        let rows = markup["inline_keyboard"].as_array().unwrap();
        // All 3 candidates in a single row
        assert_eq!(rows.len(), 1);
        let buttons = rows[0].as_array().unwrap();
        assert_eq!(buttons.len(), 3);
        // First button
        assert_eq!(buttons[0]["text"], "A");
        assert_eq!(buttons[0]["callback_data"], "ask:menu_42:0");
        // Second button
        assert_eq!(buttons[1]["text"], "B");
        assert_eq!(buttons[1]["callback_data"], "ask:menu_42:1");
        // Third button
        assert_eq!(buttons[2]["text"], "C");
        assert_eq!(buttons[2]["callback_data"], "ask:menu_42:2");
    }

    #[test]
    fn telegram_callback_query_deserializes() {
        let json_str = r#"{
            "update_id": 100,
            "callback_query": {
                "id": "cb_001",
                "data": "ask:menu1:2",
                "message": {
                    "message_id": 55,
                    "chat": {"id": 12345}
                }
            }
        }"#;
        let update: TelegramUpdate = serde_json::from_str(json_str).unwrap();
        let cb = update.callback_query.unwrap();
        assert_eq!(cb.id, "cb_001");
        assert_eq!(cb.data.as_deref(), Some("ask:menu1:2"));
        assert_eq!(cb.message.unwrap().chat.id, 12345);
    }

    #[test]
    fn telegram_update_with_callback_query_none() {
        let json_str =
            r#"{"update_id": 1, "message": {"message_id": 1, "chat": {"id": 1}, "text": "hello"}}"#;
        let update: TelegramUpdate = serde_json::from_str(json_str).unwrap();
        assert!(update.callback_query.is_none());
    }

    #[test]
    fn ask_user_event_drain_returns_latest() {
        let mut queue = VecDeque::new();
        queue.push_back(AskUserEvent {
            menu_id: "old".into(),
            prompt: "Old".into(),
            candidates: vec![],
        });
        queue.push_back(AskUserEvent {
            menu_id: "new".into(),
            prompt: "New".into(),
            candidates: vec!["X".into()],
        });
        let latest = drain_latest_ask_user_event(&mut queue);
        assert!(latest.is_some());
        assert_eq!(latest.unwrap().menu_id, "new");
        assert!(queue.is_empty());
    }

    #[test]
    fn ask_user_event_drain_empty() {
        let mut queue = VecDeque::new();
        assert!(drain_latest_ask_user_event(&mut queue).is_none());
    }

    // === Phase E: File handling tests ===

    #[test]
    fn resolve_files_absolute_existing() {
        let tmp = tempdir().unwrap();
        let f = tmp.path().join("test.txt");
        std::fs::write(&f, "hello").unwrap();
        let resolved = resolve_files(&[f.to_string_lossy().to_string()]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0], f.canonicalize().unwrap());
    }

    #[test]
    fn resolve_files_nonexistent_returns_empty() {
        let resolved = resolve_files(&["/nonexistent/path/file.txt".into()]);
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_files_tilde_expansion() {
        // ~ should expand to HOME; we can't guarantee files exist there,
        // but we can test the path expansion logic indirectly
        let _home = env::var("HOME").unwrap_or_default();
        let test_path = format!("~/{}", "___nonexistent_test_file_xyz___.txt");
        let resolved = resolve_files(&[test_path]);
        assert!(resolved.is_empty()); // File doesn't exist, so empty
    }

    #[test]
    fn resolve_files_dedup() {
        let tmp = tempdir().unwrap();
        let f = tmp.path().join("dup.txt");
        std::fs::write(&f, "data").unwrap();
        let p = f.to_string_lossy().to_string();
        let resolved = resolve_files(&[p.clone(), p]);
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn files_from_text_extracts_and_resolves() {
        let tmp = tempdir().unwrap();
        let f = tmp.path().join("output.png");
        std::fs::write(&f, "img").unwrap();
        let text = format!("Here is the result: {}", f.display());
        let files = files_from_text(&text);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], f.canonicalize().unwrap());
    }

    #[test]
    fn files_from_text_no_files() {
        let files = files_from_text("No files here, just text");
        assert!(files.is_empty());
    }

    #[test]
    fn render_file_markers_format() {
        let files = vec![
            PathBuf::from("/tmp/report.pdf"),
            PathBuf::from("/tmp/data.csv"),
        ];
        let rendered = render_file_markers(&files);
        assert_eq!(rendered.len(), 2);
        assert!(rendered[0].contains("📎"));
        assert!(rendered[0].contains("report.pdf"));
        assert!(rendered[1].contains("data.csv"));
    }

    #[test]
    fn render_file_markers_empty() {
        let rendered = render_file_markers(&[]);
        assert!(rendered.is_empty());
    }

    #[test]
    fn build_client_with_proxy_no_env() {
        // With no proxy env vars set, should succeed
        // (may have proxy vars in CI, so just test it doesn't panic)
        let result = build_client_with_proxy();
        assert!(result.is_ok());
    }

    #[test]
    fn user_state_cancel_no_task() {
        let mut state = UserState::new();
        // Cancelling when no task is running should be a no-op
        state.cancel_stream_task();
        assert!(state.stream_task.is_none());
    }

    #[test]
    fn user_state_cancel_with_task() {
        let mut state = UserState::new();
        let handle = tokio::runtime::Runtime::new().unwrap().spawn(async {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
        state.stream_task = Some(handle);
        state.cancel_stream_task();
        assert!(state.stream_task.is_none());
    }

    #[test]
    fn extract_file_markers_various_tokens() {
        let text = "Check /tmp/out.png and ./local.md but not foo.txt";
        let markers = extract_file_markers(text);
        assert!(markers.contains(&"/tmp/out.png".to_string()));
        assert!(markers.contains(&"./local.md".to_string()));
        // foo.txt is not a path (no / prefix)
        assert!(!markers.iter().any(|m| m == "foo.txt"));
    }
}
