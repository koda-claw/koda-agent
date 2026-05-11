use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use koda_agent_core::{
    AgentConfig, AgentResponse, ChatMessage, LlmClient, LlmModelConfig, LlmStreamEvent,
    LlmUsageSummary, ToolCall, auto_make_url,
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct OpenAiClient {
    http: Client,
    cfg: AgentConfig,
    last_text_tools: Arc<Mutex<String>>,
    session_overrides: Arc<Mutex<serde_json::Map<String, Value>>>,
}

impl OpenAiClient {
    pub fn new(cfg: AgentConfig) -> Self {
        let mut builder =
            Client::builder().connect_timeout(Duration::from_secs(cfg.connect_timeout_secs.max(1)));
        if cfg.timeout_secs > 0 {
            builder = builder.timeout(Duration::from_secs(cfg.timeout_secs));
        }
        if !cfg.verify_tls {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(proxy) = cfg.proxy.as_deref().filter(|s| !s.trim().is_empty()) {
            match reqwest::Proxy::all(proxy) {
                Ok(proxy) => builder = builder.proxy(proxy),
                Err(err) => tracing::warn!("invalid LLM proxy {proxy}: {err}"),
            }
        }
        let http = builder.build().unwrap_or_else(|_| Client::new());
        Self {
            http,
            cfg,
            last_text_tools: Arc::default(),
            session_overrides: Arc::default(),
        }
    }
    pub fn arc(cfg: AgentConfig) -> Arc<Self> {
        Arc::new(Self::new(cfg))
    }
    pub fn multi_arc(cfg: AgentConfig) -> Arc<MultiLlmClient> {
        Arc::new(MultiLlmClient::new(cfg))
    }
}

pub struct MultiLlmClient {
    clients: Vec<OpenAiClient>,
    names: Vec<String>,
    active_order: Vec<usize>,
    max_retries: usize,
    base_delay: Duration,
    spring_back: Duration,
    current: Mutex<usize>,
    switched_at: Mutex<Option<Instant>>,
}

impl MultiLlmClient {
    pub fn new(cfg: AgentConfig) -> Self {
        let configs = if cfg.llm_configs.is_empty() {
            vec![model_config_from_agent(&cfg)]
        } else {
            cfg.llm_configs.clone()
        };
        let names = configs
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>();
        let clients = configs
            .into_iter()
            .map(|entry| OpenAiClient::new(agent_config_for_model(&cfg, &entry)))
            .collect::<Vec<_>>();
        let active_order = resolve_mixin_order(&cfg.mixin.llm_nos, &names, &clients);
        Self {
            clients,
            names,
            active_order,
            max_retries: cfg.mixin.max_retries,
            base_delay: Duration::from_secs_f64(cfg.mixin.base_delay_secs.max(0.0)),
            spring_back: Duration::from_secs(cfg.mixin.spring_back_secs),
            current: Mutex::new(0),
            switched_at: Mutex::new(None),
        }
    }

    fn pick_start(&self) -> usize {
        let primary = self.active_order.first().copied().unwrap_or(0);
        let mut current = self.current.lock().expect("multi llm current lock");
        if *current != primary {
            let should_spring = self
                .switched_at
                .lock()
                .expect("multi llm switched_at lock")
                .is_some_and(|t| self.spring_back.is_zero() || t.elapsed() > self.spring_back);
            if should_spring {
                *current = primary;
                *self.switched_at.lock().expect("multi llm switched_at lock") = None;
            }
        }
        *current
    }

    fn remember_success(&self, idx: usize, attempted_offset: usize) {
        *self.current.lock().expect("multi llm current lock") = idx;
        if attempted_offset > 0 {
            *self.switched_at.lock().expect("multi llm switched_at lock") = Some(Instant::now());
        }
    }
}

#[async_trait]
impl LlmClient for MultiLlmClient {
    async fn chat(&self, messages: &[ChatMessage], tools_schema: &Value) -> Result<AgentResponse> {
        let len = self.active_order.len();
        if len == 0 {
            bail!("no LLM clients configured");
        }
        let start = self.pick_start();
        let base_pos = self
            .active_order
            .iter()
            .position(|idx| *idx == start)
            .unwrap_or(0);
        let mut last_error = None;
        for attempt in 0..=self.max_retries {
            let pos = (base_pos + attempt) % len;
            let idx = self.active_order[pos];
            match self.clients[idx].chat(messages, tools_schema).await {
                Ok(resp) => {
                    self.remember_success(idx, attempt);
                    return Ok(resp);
                }
                Err(err)
                    if self
                        .clients
                        .get(idx)
                        .map(|c| c.cfg.failover)
                        .unwrap_or(true)
                        && attempt < self.max_retries
                        && should_failover_error(&err) =>
                {
                    if (attempt + 1) % len == 0 && !self.base_delay.is_zero() {
                        let round = ((attempt + 1) / len).max(1) as u32;
                        let delay = self
                            .base_delay
                            .mul_f64(1.5_f64.powi(round.saturating_sub(1) as i32))
                            .min(Duration::from_secs(30));
                        tokio::time::sleep(delay).await;
                    }
                    last_error = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("no LLM clients configured")))
    }

    async fn chat_with_events(
        &self,
        messages: &[ChatMessage],
        tools_schema: &Value,
        emit: &(dyn Fn(LlmStreamEvent) + Send + Sync),
    ) -> Result<AgentResponse> {
        let len = self.active_order.len();
        if len == 0 {
            bail!("no LLM clients configured");
        }
        let start = self.pick_start();
        let base_pos = self
            .active_order
            .iter()
            .position(|idx| *idx == start)
            .unwrap_or(0);
        let mut last_error = None;
        for attempt in 0..=self.max_retries {
            let pos = (base_pos + attempt) % len;
            let idx = self.active_order[pos];
            let saw_stream_event = Arc::new(AtomicBool::new(false));
            let saw_stream_event_for_emit = Arc::clone(&saw_stream_event);
            let emit_with_marker = |event: LlmStreamEvent| {
                saw_stream_event_for_emit.store(true, Ordering::SeqCst);
                emit(event);
            };
            match self.clients[idx]
                .chat_with_events(messages, tools_schema, &emit_with_marker)
                .await
            {
                Ok(resp) => {
                    self.remember_success(idx, attempt);
                    return Ok(resp);
                }
                Err(err)
                    if !saw_stream_event.load(Ordering::SeqCst)
                        && self
                            .clients
                            .get(idx)
                            .map(|c| c.cfg.failover)
                            .unwrap_or(true)
                        && attempt < self.max_retries
                        && should_failover_error(&err) =>
                {
                    if (attempt + 1) % len == 0 && !self.base_delay.is_zero() {
                        let round = ((attempt + 1) / len).max(1) as u32;
                        let delay = self
                            .base_delay
                            .mul_f64(1.5_f64.powi(round.saturating_sub(1) as i32))
                            .min(Duration::from_secs(30));
                        tokio::time::sleep(delay).await;
                    }
                    last_error = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("no LLM clients configured")))
    }

    fn name(&self) -> String {
        let idx = *self.current.lock().expect("multi llm current lock");
        self.clients
            .get(idx)
            .map(LlmClient::name)
            .unwrap_or_else(|| "NoLLM".into())
    }

    fn set_session_option(&self, key: &str, value: &Value) -> Result<bool> {
        let mut applied = false;
        for client in &self.clients {
            applied |= client.set_session_option(key, value)?;
        }
        Ok(applied)
    }

    fn list_llms(&self) -> Vec<(usize, String, bool)> {
        let current = *self.current.lock().expect("multi llm current lock");
        self.clients
            .iter()
            .enumerate()
            .map(|(idx, c)| {
                let name = self.names.get(idx).cloned().unwrap_or_else(|| c.name());
                (idx, format!("{name} ({})", c.name()), idx == current)
            })
            .collect()
    }

    fn switch_llm(&self, n: usize) -> Result<()> {
        if n >= self.clients.len() {
            bail!(
                "LLM index out of range: {n} (configured: {})",
                self.clients.len()
            );
        }
        *self.current.lock().expect("multi llm current lock") = n;
        *self.switched_at.lock().expect("multi llm switched_at lock") = Some(Instant::now());
        Ok(())
    }
}

fn resolve_mixin_order(
    selectors: &[String],
    names: &[String],
    clients: &[OpenAiClient],
) -> Vec<usize> {
    let mut out = Vec::new();
    if selectors.is_empty() {
        out.extend(0..clients.len());
    } else {
        for selector in selectors {
            let selector = selector.trim();
            let idx = selector.parse::<usize>().ok().or_else(|| {
                names.iter().enumerate().find_map(|(idx, name)| {
                    let model = &clients[idx].cfg.openai_model;
                    (name == selector || model == selector || clients[idx].name() == selector)
                        .then_some(idx)
                })
            });
            if let Some(idx) = idx
                && idx < clients.len()
                && !out.contains(&idx)
            {
                out.push(idx);
            } else {
                tracing::warn!("unknown mixin llm selector ignored: {selector}");
            }
        }
    }
    if out.is_empty() {
        out.extend(0..clients.len());
    }
    out
}

fn model_config_from_agent(cfg: &AgentConfig) -> LlmModelConfig {
    LlmModelConfig {
        name: cfg.openai_model.clone(),
        base_url: cfg.openai_base_url.clone(),
        api_key: cfg.openai_api_key.clone(),
        model: cfg.openai_model.clone(),
        api_style: cfg.llm_api_style.clone(),
        stream: cfg.stream,
        timeout_secs: cfg.timeout_secs,
        connect_timeout_secs: cfg.connect_timeout_secs,
        verify_tls: cfg.verify_tls,
        temperature: cfg.temperature,
        max_tokens: cfg.max_tokens,
        reasoning_effort: cfg.reasoning_effort.clone(),
        thinking_type: cfg.thinking_type.clone(),
        thinking_budget_tokens: cfg.thinking_budget_tokens,
        service_tier: cfg.service_tier.clone(),
        proxy: cfg.proxy.clone(),
        failover: cfg.failover,
        custom_headers: cfg.custom_headers.clone(),
    }
}

fn agent_config_for_model(base: &AgentConfig, model: &LlmModelConfig) -> AgentConfig {
    let mut cfg = base.clone();
    cfg.openai_base_url = model.base_url.clone();
    cfg.openai_api_key = model.api_key.clone();
    cfg.openai_model = model.model.clone();
    cfg.llm_api_style = model.api_style.clone();
    cfg.stream = model.stream;
    cfg.timeout_secs = model.timeout_secs;
    cfg.connect_timeout_secs = model.connect_timeout_secs;
    cfg.verify_tls = model.verify_tls;
    cfg.temperature = model.temperature;
    cfg.max_tokens = model.max_tokens;
    cfg.reasoning_effort = model.reasoning_effort.clone();
    cfg.thinking_type = model.thinking_type.clone();
    cfg.thinking_budget_tokens = model.thinking_budget_tokens;
    cfg.service_tier = model.service_tier.clone();
    cfg.proxy = model.proxy.clone();
    cfg.failover = model.failover;
    cfg.custom_headers = model.custom_headers.clone();
    cfg
}

fn should_failover_error(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    text.contains("timeout")
        || text.contains("operation timed out")
        || text.contains("429")
        || text.contains("500")
        || text.contains("502")
        || text.contains("503")
        || text.contains("504")
        || text.contains("connect")
}

fn trim_messages_history(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out = messages.to_vec();
    compress_history_tags(&mut out, 10, 800);
    let context_win = std::env::var("KODA_CONTEXT_WINDOW_CHARS")
        .or_else(|_| std::env::var("KODA_CONTEXT_WIN_CHARS"))
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(28_000);
    let limit = context_win.saturating_mul(3);
    let mut cost = messages_cost(&out);
    if cost <= limit {
        return out;
    }
    compress_history_tags(&mut out, 4, 800);
    let target = limit.saturating_mul(6) / 10;
    while out.len() > 5 && cost > target {
        let remove_at = usize::from(out.first().is_some_and(|m| m.role == "system"));
        if remove_at >= out.len() {
            break;
        }
        out.remove(remove_at);
        while out.len() > 5
            && out
                .get(remove_at)
                .is_some_and(|msg| msg.role.as_str() != "user")
        {
            out.remove(remove_at);
        }
        if let Some(msg) = out.get_mut(remove_at)
            && msg.role == "user"
        {
            sanitize_leading_user_msg(msg);
        }
        cost = messages_cost(&out);
    }
    out
}

fn messages_cost(messages: &[ChatMessage]) -> usize {
    serde_json::to_string(messages)
        .map(|s| s.chars().count())
        .unwrap_or_default()
}

fn compress_history_tags(messages: &mut [ChatMessage], keep_recent: usize, max_len: usize) {
    let cutoff = messages.len().saturating_sub(keep_recent);
    for msg in messages.iter_mut().take(cutoff) {
        compress_value(&mut msg.content, max_len);
    }
}

fn compress_value(value: &mut Value, max_len: usize) {
    match value {
        Value::String(s) => *s = compress_text_tags(s, max_len),
        Value::Array(arr) => {
            for value in arr {
                compress_value(value, max_len);
            }
        }
        Value::Object(obj) => {
            for value in obj.values_mut() {
                compress_value(value, max_len);
            }
        }
        _ => {}
    }
}

fn compress_text_tags(text: &str, max_len: usize) -> String {
    let mut out = text.to_string();
    for tag in ["history", "key_info", "earlier_context"] {
        out = replace_tag_body(&out, tag, |_| "[...]".to_string());
    }
    for tag in ["thinking", "think", "tool_use", "tool_result"] {
        out = replace_tag_body(&out, tag, |body| truncate_middle(body, max_len));
    }
    truncate_middle(&out, max_len.saturating_mul(4))
}

fn replace_tag_body(text: &str, tag: &str, f: impl Fn(&str) -> String) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        let body_start = start + open.len();
        let Some(end_rel) = rest[body_start..].find(&close) else {
            break;
        };
        let body_end = body_start + end_rel;
        out.push_str(&rest[..body_start]);
        out.push_str(&f(&rest[body_start..body_end]));
        out.push_str(&close);
        rest = &rest[body_end + close.len()..];
    }
    out.push_str(rest);
    out
}

fn truncate_middle(text: &str, max_len: usize) -> String {
    let len = text.chars().count();
    if len <= max_len || max_len < 20 {
        return text.to_string();
    }
    let side = max_len / 2;
    let head = text.chars().take(side).collect::<String>();
    let tail = text
        .chars()
        .skip(len.saturating_sub(side))
        .collect::<String>();
    format!("{head}\n...[Truncated]...\n{tail}")
}

fn sanitize_leading_user_msg(msg: &mut ChatMessage) {
    let Some(results) = tool_results_from_content(&msg.content) else {
        return;
    };
    let mut parts = results
        .into_iter()
        .map(|tr| tool_content_string(&tr.content))
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>();
    if let Some(content) = msg.content.get("content").map(message_text)
        && !content.trim().is_empty()
    {
        parts.push(content);
    }
    msg.content = Value::String(parts.join("\n"));
}

#[async_trait]
impl LlmClient for OpenAiClient {
    async fn chat(&self, messages: &[ChatMessage], tools_schema: &Value) -> Result<AgentResponse> {
        self.chat_inner(messages, tools_schema, &|_| {}).await
    }

    async fn chat_with_events(
        &self,
        messages: &[ChatMessage],
        tools_schema: &Value,
        emit: &(dyn Fn(LlmStreamEvent) + Send + Sync),
    ) -> Result<AgentResponse> {
        self.chat_inner(messages, tools_schema, emit).await
    }

    fn name(&self) -> String {
        format!("{}/{}", self.cfg.llm_api_style, self.cfg.openai_model)
    }

    fn set_session_option(&self, key: &str, value: &Value) -> Result<bool> {
        if is_supported_llm_session_option(key) {
            self.session_overrides
                .lock()
                .expect("llm session override lock")
                .insert(key.to_string(), value.clone());
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

impl OpenAiClient {
    async fn chat_inner(
        &self,
        messages: &[ChatMessage],
        tools_schema: &Value,
        emit: &(dyn Fn(LlmStreamEvent) + Send + Sync),
    ) -> Result<AgentResponse> {
        let overrides = self
            .session_overrides
            .lock()
            .expect("llm session override lock")
            .clone();
        if !overrides.is_empty() {
            let mut cfg = self.cfg.clone();
            apply_session_overrides_to_cfg(&mut cfg, &overrides);
            let scoped = OpenAiClient {
                http: self.http.clone(),
                cfg,
                last_text_tools: Arc::clone(&self.last_text_tools),
                session_overrides: Arc::default(),
            };
            return Box::pin(scoped.chat_inner(messages, tools_schema, emit)).await;
        }
        let messages = trim_messages_history(messages);
        if self.cfg.llm_api_style.eq_ignore_ascii_case("claude")
            || self.cfg.llm_api_style.eq_ignore_ascii_case("messages")
        {
            return self.claude_messages(&messages, tools_schema, emit).await;
        }
        if self.cfg.llm_api_style.eq_ignore_ascii_case("responses") {
            return self.responses(&messages, tools_schema, emit).await;
        }
        if self.cfg.llm_api_style.eq_ignore_ascii_case("text")
            || self.cfg.llm_api_style.eq_ignore_ascii_case("tool")
            || self.cfg.llm_api_style.eq_ignore_ascii_case("text_protocol")
        {
            return self.text_protocol_chat(&messages, tools_schema, emit).await;
        }
        let url = auto_make_url(&self.cfg.openai_base_url, "chat/completions");
        let mut oai_messages = openai_chat_messages(&messages);
        stamp_oai_cache_markers(&mut oai_messages, &self.cfg.openai_model);
        let mut payload = json!({
            "model": self.cfg.openai_model,
            "messages": oai_messages,
            "tools": tools_schema,
            "stream": self.cfg.stream
        });
        apply_common_openai_options(&mut payload, &self.cfg, false);
        let res = self
            .post_json_with_retry(&url, &payload)
            .await
            .context("send OpenAI-compatible request")?;
        let status = res.status();
        if self.cfg.stream {
            let text = collect_sse_text_with_events(res, |line| {
                emit_openai_chat_sse_delta(line, emit, &self.cfg.openai_model);
            })
            .await?;
            if !status.is_success() {
                return Err(anyhow!("LLM error {status}: {text}"));
            }
            record_sse_usage(&self.cfg, "chat_completions", &text)?;
            let lines = text.lines().collect::<Vec<_>>();
            return parse_openai_sse_lines(&lines);
        }
        let body = response_json(res, status).await?;
        if !status.is_success() {
            return Err(anyhow!("LLM error {status}: {body}"));
        }
        record_usage(
            &self.cfg,
            "chat_completions",
            body.get("usage").unwrap_or(&Value::Null),
        )?;
        emit_usage_if_present(
            "chat_completions",
            &self.cfg.openai_model,
            body.get("usage"),
            emit,
        );
        parse_openai_chat_json(&body)
    }
}

fn is_supported_llm_session_option(key: &str) -> bool {
    matches!(
        key,
        "temperature"
            | "max_tokens"
            | "reasoning_effort"
            | "service_tier"
            | "thinking_type"
            | "thinking_budget_tokens"
            | "stream"
    )
}

fn apply_session_overrides_to_cfg(
    cfg: &mut AgentConfig,
    overrides: &serde_json::Map<String, Value>,
) {
    for (key, value) in overrides {
        match key.as_str() {
            "temperature" => cfg.temperature = value.as_f64(),
            "max_tokens" => cfg.max_tokens = value.as_u64(),
            "reasoning_effort" => {
                cfg.reasoning_effort = value
                    .as_str()
                    .map(str::trim)
                    .map(str::to_ascii_lowercase)
                    .filter(|v| {
                        matches!(
                            v.as_str(),
                            "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
                        )
                    });
            }
            "service_tier" => {
                cfg.service_tier = value
                    .as_str()
                    .map(str::trim)
                    .map(str::to_ascii_lowercase)
                    .filter(|v| matches!(v.as_str(), "auto" | "default" | "priority" | "flex"));
            }
            "thinking_type" => {
                cfg.thinking_type = value
                    .as_str()
                    .map(str::trim)
                    .map(str::to_ascii_lowercase)
                    .filter(|v| matches!(v.as_str(), "adaptive" | "enabled" | "disabled"));
            }
            "thinking_budget_tokens" => cfg.thinking_budget_tokens = value.as_u64(),
            "stream" => cfg.stream = value.as_bool().unwrap_or(cfg.stream),
            _ => {}
        }
    }
}

fn openai_chat_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    for (idx, m) in messages.iter().enumerate() {
        match m.role.as_str() {
            "system" | "user" => {
                if let Some(results) = tool_results_from_content(&m.content) {
                    for tr in results {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tr.tool_call_id.unwrap_or_default(),
                            "content": tool_content_string(&tr.content),
                        }));
                    }
                    let prompt = m
                        .content
                        .get("content")
                        .map(message_text)
                        .unwrap_or_default();
                    if !prompt.trim().is_empty() {
                        out.push(json!({"role":"user","content":prompt}));
                    }
                } else {
                    out.push(json!({"role":m.role,"content":message_text(&m.content)}));
                }
            }
            "assistant" => {
                if let Some(tool_calls) = tool_calls_from_content(&m.content) {
                    let following_results = messages
                        .get(idx + 1)
                        .and_then(|next| tool_results_from_content(&next.content))
                        .unwrap_or_default();
                    let mut msg = json!({
                        "role":"assistant",
                        "content":m.content.get("text").and_then(Value::as_str).unwrap_or_default(),
                        "tool_calls":calls_from_tool_calls(&tool_calls),
                    });
                    if let Some(thinking) = thinking_from_content(&m.content)
                        && let Some(obj) = msg.as_object_mut()
                    {
                        obj.insert("reasoning_content".into(), Value::String(thinking));
                    }
                    out.push(msg);
                    for missing in missing_tool_results(&tool_calls, &following_results) {
                        out.push(json!({
                            "role":"tool",
                            "tool_call_id": missing.id.unwrap_or_default(),
                            "content": format!(r#"{{"status":"error","msg":"tool result missing for {}"}}"#, missing.name),
                        }));
                    }
                } else {
                    let mut msg = json!({"role":"assistant","content":message_text(&m.content)});
                    if let Some(thinking) = thinking_from_content(&m.content)
                        && let Some(obj) = msg.as_object_mut()
                    {
                        obj.insert("reasoning_content".into(), Value::String(thinking));
                    }
                    out.push(msg);
                }
            }
            _ => {}
        }
    }
    out
}

fn missing_tool_results(
    tool_calls: &[ToolCall],
    results: &[koda_agent_core::ToolResult],
) -> Vec<ToolCall> {
    tool_calls
        .iter()
        .filter(|tc| {
            let Some(id) = tc.id.as_deref() else {
                return false;
            };
            !results
                .iter()
                .any(|tr| tr.tool_call_id.as_deref() == Some(id))
        })
        .cloned()
        .collect()
}

fn calls_from_tool_calls(tool_calls: &[ToolCall]) -> Vec<Value> {
    tool_calls
        .iter()
        .enumerate()
        .map(|(idx, tc)| {
            json!({
                "id": tc.id.clone().unwrap_or_else(|| format!("call_{idx}")),
                "type": "function",
                "function": {
                    "name": tc.name,
                    "arguments": serde_json::to_string(&tc.args).unwrap_or_else(|_| "{}".into())
                }
            })
        })
        .collect()
}

fn stamp_oai_cache_markers(messages: &mut [Value], model: &str) {
    let ml = model.to_ascii_lowercase();
    if !ml.contains("claude") && !ml.contains("anthropic") {
        return;
    }
    let user_idxs = messages
        .iter()
        .enumerate()
        .filter_map(|(idx, msg)| {
            (msg.get("role").and_then(Value::as_str) == Some("user")).then_some(idx)
        })
        .collect::<Vec<_>>();
    for idx in user_idxs.into_iter().rev().take(2) {
        let Some(obj) = messages[idx].as_object_mut() else {
            continue;
        };
        let Some(content) = obj.get_mut("content") else {
            continue;
        };
        match content {
            Value::String(text) => {
                *content =
                    json!([{"type":"text","text":text,"cache_control":{"type":"ephemeral"}}]);
            }
            Value::Array(arr) if !arr.is_empty() => {
                if let Some(last) = arr.last_mut().and_then(Value::as_object_mut) {
                    last.insert("cache_control".into(), json!({"type":"ephemeral"}));
                }
            }
            _ => {}
        }
    }
}

fn thinking_from_content(content: &Value) -> Option<String> {
    content
        .get("thinking")
        .or_else(|| content.get("reasoning_content"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn tool_calls_from_content(content: &Value) -> Option<Vec<ToolCall>> {
    let calls = content.get("tool_calls")?.as_array()?;
    Some(
        calls
            .iter()
            .filter_map(|v| serde_json::from_value::<ToolCall>(v.clone()).ok())
            .collect(),
    )
    .filter(|v: &Vec<ToolCall>| !v.is_empty())
}

fn tool_results_from_content(content: &Value) -> Option<Vec<koda_agent_core::ToolResult>> {
    let results = content.get("tool_results")?.as_array()?;
    Some(
        results
            .iter()
            .filter_map(|v| serde_json::from_value::<koda_agent_core::ToolResult>(v.clone()).ok())
            .collect(),
    )
    .filter(|v: &Vec<koda_agent_core::ToolResult>| !v.is_empty())
}

fn tool_content_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

impl OpenAiClient {
    async fn text_protocol_chat(
        &self,
        messages: &[ChatMessage],
        tools_schema: &Value,
        emit: &(dyn Fn(LlmStreamEvent) + Send + Sync),
    ) -> Result<AgentResponse> {
        let url = auto_make_url(&self.cfg.openai_base_url, "chat/completions");
        let prompt_messages = self.text_protocol_messages(messages, tools_schema);
        let mut payload = json!({
            "model": self.cfg.openai_model,
            "messages": prompt_messages,
            "stream": self.cfg.stream
        });
        apply_common_openai_options(&mut payload, &self.cfg, false);
        let res = self
            .post_json_with_retry(&url, &payload)
            .await
            .context("send text-protocol OpenAI-compatible request")?;
        let status = res.status();
        let response = if self.cfg.stream {
            let text = collect_sse_text_with_events(res, |line| {
                emit_openai_chat_sse_delta(line, emit, &self.cfg.openai_model);
            })
            .await?;
            if !status.is_success() {
                return Err(anyhow!("LLM error {status}: {text}"));
            }
            record_sse_usage(&self.cfg, "chat_completions", &text)?;
            let lines = text.lines().collect::<Vec<_>>();
            parse_openai_sse_lines(&lines)?
        } else {
            let body = response_json(res, status).await?;
            if !status.is_success() {
                return Err(anyhow!("LLM error {status}: {body}"));
            }
            record_usage(
                &self.cfg,
                "chat_completions",
                body.get("usage").unwrap_or(&Value::Null),
            )?;
            emit_usage_if_present(
                "chat_completions",
                &self.cfg.openai_model,
                body.get("usage"),
                emit,
            );
            parse_openai_chat_json(&body)?
        };
        Ok(parse_text_protocol_response(response))
    }

    fn text_protocol_messages(&self, messages: &[ChatMessage], tools_schema: &Value) -> Vec<Value> {
        let tools_json = serde_json::to_string(tools_schema).unwrap_or_else(|_| "[]".into());
        let reuse_tools = {
            let mut last = self
                .last_text_tools
                .lock()
                .expect("text protocol tool cache lock");
            let reuse = *last == tools_json;
            if !reuse {
                *last = tools_json.clone();
            }
            reuse
        };
        text_protocol_messages_with_instruction(
            messages,
            &text_tool_instruction_from_json(&tools_json, reuse_tools),
        )
    }

    async fn claude_messages(
        &self,
        messages: &[ChatMessage],
        tools_schema: &Value,
        emit: &(dyn Fn(LlmStreamEvent) + Send + Sync),
    ) -> Result<AgentResponse> {
        let url = auto_make_url(&self.cfg.openai_base_url, "messages");
        let (system, claude_messages) = claude_messages_input(messages, &self.cfg.openai_model);
        let system = claude_system_value(&system);
        let (model, beta) = claude_model_and_beta(&self.cfg.openai_model);
        let mut payload = json!({
            "model": model,
            "system": system,
            "messages": claude_messages,
            "tools": claude_tools(tools_schema),
            "stream": self.cfg.stream,
            "max_tokens": self.cfg.max_tokens.unwrap_or(8192)
        });
        apply_claude_options(&mut payload, &self.cfg);
        let req = self
            .http
            .post(url)
            .header(
                if self.cfg.openai_api_key.starts_with("sk-ant-") {
                    "x-api-key"
                } else {
                    "authorization"
                },
                if self.cfg.openai_api_key.starts_with("sk-ant-") {
                    self.cfg.openai_api_key.clone()
                } else {
                    format!("Bearer {}", self.cfg.openai_api_key)
                },
            )
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", beta);
        let req = apply_custom_headers(req, &self.cfg).json(&payload);
        let res = req
            .send()
            .await
            .context("send Claude Messages-compatible request")?;
        let status = res.status();
        if self.cfg.stream {
            let text = collect_sse_text_with_events(res, |line| {
                emit_claude_sse_delta(line, emit, &self.cfg.openai_model);
            })
            .await?;
            if !status.is_success() {
                return Err(anyhow!("LLM error {status}: {text}"));
            }
            record_sse_usage(&self.cfg, "messages", &text)?;
            let lines = text.lines().collect::<Vec<_>>();
            return parse_claude_sse_lines(&lines);
        }
        let body = response_json(res, status).await?;
        if !status.is_success() {
            return Err(anyhow!("LLM error {status}: {body}"));
        }
        record_usage(
            &self.cfg,
            "messages",
            body.get("usage").unwrap_or(&Value::Null),
        )?;
        emit_usage_if_present("messages", &self.cfg.openai_model, body.get("usage"), emit);
        parse_claude_messages_json(&body)
    }

    async fn responses(
        &self,
        messages: &[ChatMessage],
        tools_schema: &Value,
        emit: &(dyn Fn(LlmStreamEvent) + Send + Sync),
    ) -> Result<AgentResponse> {
        let url = auto_make_url(&self.cfg.openai_base_url, "responses");
        let mut payload = json!({
            "model": self.cfg.openai_model,
            "input": responses_input(messages),
            "tools": responses_tools(tools_schema),
            "stream": self.cfg.stream
        });
        apply_common_openai_options(&mut payload, &self.cfg, true);
        let res = self
            .post_json_with_retry(&url, &payload)
            .await
            .context("send OpenAI Responses-compatible request")?;
        let status = res.status();
        if self.cfg.stream {
            let text = collect_sse_text_with_events(res, |line| {
                emit_responses_sse_delta(line, emit, &self.cfg.openai_model);
            })
            .await?;
            if !status.is_success() {
                return Err(anyhow!("LLM error {status}: {text}"));
            }
            record_sse_usage(&self.cfg, "responses", &text)?;
            let lines = text.lines().collect::<Vec<_>>();
            return parse_responses_sse_lines(&lines);
        }
        let body = response_json(res, status).await?;
        if !status.is_success() {
            return Err(anyhow!("LLM error {status}: {body}"));
        }
        record_usage(
            &self.cfg,
            "responses",
            body.get("usage").unwrap_or(&Value::Null),
        )?;
        emit_usage_if_present("responses", &self.cfg.openai_model, body.get("usage"), emit);
        parse_responses_json(&body)
    }
}

impl OpenAiClient {
    async fn post_json_with_retry(&self, url: &str, payload: &Value) -> Result<reqwest::Response> {
        let attempts = std::env::var("KODA_LLM_RETRIES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(3)
            .max(1);
        let mut last_error = None;
        for attempt in 1..=attempts {
            let req = self.http.post(url).bearer_auth(&self.cfg.openai_api_key);
            match apply_custom_headers(req, &self.cfg)
                .json(payload)
                .send()
                .await
            {
                Ok(response) if should_retry_status(response.status()) && attempt < attempts => {
                    let delay = retry_delay(attempt);
                    let _ = response.bytes().await;
                    tokio::time::sleep(delay).await;
                }
                Ok(response) => return Ok(response),
                Err(err) if attempt < attempts && (err.is_timeout() || err.is_connect()) => {
                    last_error = Some(err);
                    tokio::time::sleep(retry_delay(attempt)).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        Err(last_error
            .map(anyhow::Error::from)
            .unwrap_or_else(|| anyhow!("LLM request failed after {attempts} attempts")))
    }
}

fn apply_common_openai_options(payload: &mut Value, cfg: &AgentConfig, responses: bool) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    if let Some(temp) = cfg.temperature {
        obj.insert(
            "temperature".into(),
            json!(normalize_temperature(temp, &cfg.openai_model)),
        );
    }
    if let Some(tokens) = cfg.max_tokens {
        let key = if responses {
            "max_output_tokens"
        } else if cfg.openai_model.to_ascii_lowercase().starts_with("gpt-5")
            || cfg.openai_model.to_ascii_lowercase().starts_with("o1")
            || cfg.openai_model.to_ascii_lowercase().starts_with("o2")
            || cfg.openai_model.to_ascii_lowercase().starts_with("o3")
            || cfg.openai_model.to_ascii_lowercase().starts_with("o4")
        {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        obj.insert(key.into(), json!(tokens));
    }
    if let Some(effort) = &cfg.reasoning_effort {
        if responses {
            obj.insert("reasoning".into(), json!({"effort": effort}));
        } else {
            obj.insert("reasoning_effort".into(), json!(effort));
        }
    }
    if let Some(tier) = &cfg.service_tier {
        obj.insert("service_tier".into(), json!(tier));
    }
    if cfg.stream && !responses {
        obj.insert("stream_options".into(), json!({"include_usage": true}));
    }
}

fn normalize_temperature(temp: f64, model: &str) -> f64 {
    let ml = model.to_ascii_lowercase();
    if ml.contains("kimi") || ml.contains("moonshot") {
        1.0
    } else if ml.contains("minimax") {
        temp.clamp(0.01, 1.0)
    } else {
        temp
    }
}

fn apply_claude_options(payload: &mut Value, cfg: &AgentConfig) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    if let Some(temp) = cfg.temperature {
        obj.insert("temperature".into(), json!(temp));
    }
    if let Some(thinking_type) = cfg.thinking_type.as_deref() {
        let mut thinking = serde_json::Map::new();
        thinking.insert("type".into(), json!(thinking_type));
        if thinking_type == "enabled" {
            if let Some(tokens) = cfg.thinking_budget_tokens {
                thinking.insert("budget_tokens".into(), json!(tokens));
                obj.insert("thinking".into(), Value::Object(thinking));
            } else {
                tracing::warn!(
                    "thinking_type='enabled' requires thinking_budget_tokens; ignoring thinking"
                );
            }
        } else {
            obj.insert("thinking".into(), Value::Object(thinking));
        }
    }
    if let Some(effort) = cfg.reasoning_effort.as_deref() {
        let mapped = match effort {
            "low" | "medium" | "high" => Some(effort),
            "xhigh" => Some("max"),
            _ => None,
        };
        if let Some(mapped) = mapped {
            obj.insert("output_config".into(), json!({"effort": mapped}));
        }
    }
}

fn claude_model_and_beta(model: &str) -> (String, String) {
    let mut beta = vec!["prompt-caching-2024-07-31"];
    let mut cleaned = model.to_string();
    if cleaned.to_ascii_lowercase().contains("[1m]") {
        beta.push("context-1m-2025-08-07");
        cleaned = cleaned.replace("[1m]", "").replace("[1M]", "");
    }
    (cleaned, beta.join(","))
}

fn apply_custom_headers(
    mut req: reqwest::RequestBuilder,
    cfg: &AgentConfig,
) -> reqwest::RequestBuilder {
    for (k, v) in &cfg.custom_headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req
}

fn should_retry_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(250 * 2_u64.saturating_pow(attempt.saturating_sub(1) as u32))
}

fn responses_input(messages: &[ChatMessage]) -> Value {
    let mut result = Vec::new();
    for msg in openai_chat_messages(messages) {
        match msg.get("role").and_then(Value::as_str).unwrap_or("user") {
            "tool" => {
                result.push(json!({
                    "type": "function_call_output",
                    "call_id": msg.get("tool_call_id").and_then(Value::as_str).unwrap_or_default(),
                    "output": msg.get("content").and_then(Value::as_str).unwrap_or_default()
                }));
            }
            role => {
                let role = if role == "system" { "developer" } else { role };
                let text_type = if role == "assistant" {
                    "output_text"
                } else {
                    "input_text"
                };
                let text = msg
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                result.push(json!({"role":role,"content":[{"type":text_type,"text":text}]}));
                if role == "assistant"
                    && let Some(reasoning) = msg.get("reasoning_content").and_then(Value::as_str)
                {
                    result.push(json!({"type":"reasoning","summary":[{"type":"summary_text","text":reasoning}]}));
                }
                if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        result.push(json!({
                            "type":"function_call",
                            "call_id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
                            "name": call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default(),
                            "arguments": call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}")
                        }));
                    }
                }
            }
        }
    }
    Value::Array(result)
}

fn claude_messages_input(messages: &[ChatMessage], model: &str) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut out = Vec::new();
    for msg in messages {
        match msg.role.as_str() {
            "system" => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&message_text(&msg.content));
            }
            "assistant" => {
                let mut blocks = Vec::new();
                if let Some(block) = claude_thinking_block_from_content(&msg.content) {
                    blocks.push(block);
                }
                let text = msg
                    .content
                    .get("text")
                    .map(message_text)
                    .unwrap_or_else(|| message_text(&msg.content));
                if !text.trim().is_empty() {
                    blocks.push(json!({"type":"text","text":text}));
                }
                if let Some(calls) = tool_calls_from_content(&msg.content) {
                    for (idx, tc) in calls.into_iter().enumerate() {
                        blocks.push(json!({
                            "type":"tool_use",
                            "id":tc.id.unwrap_or_else(|| format!("toolu_{idx}")),
                            "name":tc.name,
                            "input":tc.args
                        }));
                    }
                }
                if !blocks.is_empty() {
                    out.push(json!({"role":"assistant","content":blocks}));
                }
            }
            "user" => {
                let mut blocks = Vec::new();
                if let Some(results) = tool_results_from_content(&msg.content) {
                    for tr in results {
                        blocks.push(json!({
                            "type":"tool_result",
                            "tool_use_id":tr.tool_call_id.unwrap_or_default(),
                            "content":tool_content_string(&tr.content)
                        }));
                    }
                    let prompt = msg
                        .content
                        .get("content")
                        .map(message_text)
                        .unwrap_or_default();
                    if !prompt.trim().is_empty() {
                        blocks.push(json!({"type":"text","text":prompt}));
                    }
                } else {
                    blocks.push(json!({"type":"text","text":message_text(&msg.content)}));
                }
                out.push(json!({"role":"user","content":blocks}));
            }
            _ => {}
        }
    }
    drop_unsigned_thinking(&mut out);
    ensure_thinking_blocks_for_model(&mut out, model);
    stamp_claude_cache_markers(&mut out);
    (system, out)
}

fn claude_thinking_block_from_content(content: &Value) -> Option<Value> {
    if let Some(thinking) = thinking_from_content(content)
        && let Some(signature) = content
            .get("thinking_signature")
            .or_else(|| content.get("signature"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
    {
        return Some(json!({"type":"thinking","thinking":thinking,"signature":signature}));
    }
    let blocks = content
        .get("raw")
        .and_then(|raw| raw.get("content"))
        .and_then(Value::as_array)
        .or_else(|| content.get("content").and_then(Value::as_array))?;
    blocks.iter().find_map(|block| {
        (block.get("type").and_then(Value::as_str) == Some("thinking")
            && block
                .get("signature")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty()))
        .then(|| block.clone())
    })
}

fn claude_system_value(system: &str) -> Value {
    if system.trim().is_empty() {
        Value::String(String::new())
    } else {
        json!([{"type":"text","text":system,"cache_control":{"type":"persistent"}}])
    }
}

fn stamp_claude_cache_markers(messages: &mut [Value]) {
    let user_idxs = messages
        .iter()
        .enumerate()
        .filter_map(|(idx, msg)| {
            (msg.get("role").and_then(Value::as_str) == Some("user")).then_some(idx)
        })
        .collect::<Vec<_>>();
    for idx in user_idxs.into_iter().rev().take(2) {
        let Some(blocks) = messages[idx]
            .get_mut("content")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        if let Some(last) = blocks.last_mut().and_then(Value::as_object_mut) {
            last.insert("cache_control".into(), json!({"type":"ephemeral"}));
        }
    }
}

fn drop_unsigned_thinking(messages: &mut [Value]) {
    for msg in messages {
        let Some(blocks) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        blocks.retain(|block| {
            block.get("type").and_then(Value::as_str) != Some("thinking")
                || block
                    .get("signature")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty())
        });
    }
}

fn ensure_thinking_blocks_for_model(messages: &mut [Value], model: &str) {
    if !model.to_ascii_lowercase().contains("deepseek") {
        return;
    }
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let has_thinking = blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("thinking"));
        if !has_thinking {
            blocks.insert(
                0,
                json!({"type":"thinking","thinking":"...","signature":"placeholder"}),
            );
        }
    }
}

fn claude_tools(tools_schema: &Value) -> Value {
    let Some(arr) = tools_schema.as_array() else {
        return tools_schema.clone();
    };
    Value::Array(
        arr.iter()
            .map(|tool| {
                if tool.get("type").and_then(Value::as_str) == Some("function")
                    && tool.get("function").is_some()
                {
                    let f = &tool["function"];
                    json!({
                        "name": f.get("name").cloned().unwrap_or(Value::String(String::new())),
                        "description": f.get("description").cloned().unwrap_or(Value::String(String::new())),
                        "input_schema": f.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}}))
                    })
                } else {
                    tool.clone()
                }
            })
            .collect(),
    )
}

#[cfg(test)]
fn text_protocol_messages(messages: &[ChatMessage], tools_schema: &Value) -> Vec<Value> {
    let tools_json = serde_json::to_string(tools_schema).unwrap_or_else(|_| "[]".into());
    text_protocol_messages_with_instruction(
        messages,
        &text_tool_instruction_from_json(&tools_json, false),
    )
}

fn text_protocol_messages_with_instruction(
    messages: &[ChatMessage],
    tool_instruction: &str,
) -> Vec<Value> {
    let mut system = String::new();
    let mut user = String::new();
    for msg in messages {
        if msg.role.eq_ignore_ascii_case("system") {
            system.push_str(&message_text(&msg.content));
            system.push('\n');
            continue;
        }
        let role = if msg.role == "assistant" {
            "ASSISTANT"
        } else {
            "USER"
        };
        user.push_str(&format!("=== {role} ===\n"));
        if let Some(results) = tool_results_from_content(&msg.content) {
            for tr in results {
                user.push_str("<tool_result>");
                user.push_str(&tool_content_string(&tr.content));
                user.push_str("</tool_result>\n");
            }
            user.push_str(
                &msg.content
                    .get("content")
                    .map(message_text)
                    .unwrap_or_default(),
            );
        } else {
            user.push_str(&message_text(&msg.content));
        }
        user.push('\n');
    }
    user.push_str("=== ASSISTANT ===\n");
    vec![
        json!({"role":"system","content":format!("{system}\n{tool_instruction}")}),
        json!({"role":"user","content":user}),
    ]
}

fn text_tool_instruction_from_json(tools: &str, reuse_tools: bool) -> String {
    if reuse_tools {
        return "\n### 工具库状态：持续有效（code_run/file_read等），**可正常调用**。调用协议沿用。\n"
            .into();
    }
    format!(
        "### 交互协议 (必须严格遵守，持续有效)\n\
1. 在 <thinking> 标签中分析现状和策略。\n\
2. 在 <summary> 中输出极短单行物理快照。\n\
3. 如需调用工具，请在正文后输出一个或多个 <tool_use>{{\"name\":\"tool_name\",\"arguments\":{{...}}}}</tool_use>，然后停止。\n\n\
Format: ```<tool_use>{{\"name\":\"tool_name\",\"arguments\":{{...}}}}</tool_use>```\n\n\
### Tools (mounted, always in effect):\n{tools}\n"
    )
}

fn parse_text_protocol_response(mut response: AgentResponse) -> AgentResponse {
    let mut text = response.content.clone();
    let thinking = extract_tag_once(&mut text, "thinking")
        .or_else(|| extract_tag_once(&mut text, "think"))
        .unwrap_or_default();
    let tool_calls = parse_text_tool_calls(&mut text);
    if !thinking.is_empty() {
        response.thinking = thinking;
    }
    if !tool_calls.is_empty() {
        response.tool_calls = tool_calls;
    }
    response.content = text.trim().to_string();
    response
}

fn extract_tag_once(text: &mut String, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)?;
    let body_start = start + open.len();
    let end_rel = text[body_start..].find(&close)?;
    let body_end = body_start + end_rel;
    let body = text[body_start..body_end].trim().to_string();
    text.replace_range(start..body_end + close.len(), "");
    Some(body)
}

fn parse_text_tool_calls(text: &mut String) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut errors = Vec::new();
    for tag in ["tool_use", "tool_call"] {
        loop {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            let Some(start) = text.find(&open) else {
                break;
            };
            let body_start = start + open.len();
            let Some(end_rel) = text[body_start..].find(&close) else {
                break;
            };
            let body_end = body_start + end_rel;
            let raw = strip_code_fence(text[body_start..body_end].trim());
            if let Some(call) = parse_text_tool_json(&raw) {
                calls.push(call);
            } else {
                errors.push(format!(
                    "Failed to parse tool_use JSON: {}",
                    smart_preview(&raw, 200)
                ));
            }
            text.replace_range(start..body_end + close.len(), "");
        }
    }
    if calls.is_empty()
        && text.contains("<tool_use>")
        && let Some(raw) = text.split("<tool_use>").nth(1).map(str::trim)
    {
        let raw = raw.trim_matches(&['>', '<'][..]);
        let candidate = strip_code_fence(raw);
        if let Some(call) = parse_text_tool_json(&candidate) {
            calls.push(call);
            if let Some(start) = text.find("<tool_use>") {
                text.replace_range(start.., "");
            }
        } else if raw.contains('{') {
            errors.push(format!(
                "Failed to parse tool_use JSON: {}",
                smart_preview(raw, 200)
            ));
        }
    }
    if calls.is_empty()
        && text.contains("\"name\"")
        && (text.contains("\"arguments\"") || text.contains("\"args\""))
        && let Some((range, raw)) = find_json_object_containing(text, "\"name\"")
        && let Some(call) = parse_text_tool_json(raw)
    {
        calls.push(call);
        text.replace_range(range, "");
    }
    if calls.is_empty() && !errors.is_empty() {
        calls.push(ToolCall {
            id: None,
            name: "bad_json".into(),
            args: json!({"msg": errors.join("\n")}),
        });
    }
    calls
}

fn strip_code_fence(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed.strip_prefix("```") {
        inner
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end_matches("```")
            .trim()
            .to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_text_tool_json(raw: &str) -> Option<ToolCall> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let id = value.get("id").and_then(Value::as_str).map(str::to_string);
    let name = value
        .get("name")
        .or_else(|| value.get("function"))
        .or_else(|| value.get("tool"))
        .and_then(Value::as_str)?
        .to_string();
    let args = value
        .get("arguments")
        .or_else(|| value.get("args"))
        .or_else(|| value.get("input"))
        .or_else(|| value.get("params"))
        .or_else(|| value.get("parameters"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    Some(ToolCall { id, name, args })
}

fn find_json_object_containing<'a>(
    text: &'a str,
    needle: &str,
) -> Option<(std::ops::Range<usize>, &'a str)> {
    let needle_at = text.find(needle)?;
    let start = text[..needle_at].rfind('{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some((start..end, &text[start..end]));
                }
            }
            _ => {}
        }
    }
    None
}

fn message_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("content")
            .or_else(|| o.get("text"))
            .map(message_text)
            .unwrap_or_else(|| value.to_string()),
        Value::Array(arr) => arr.iter().map(message_text).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

fn responses_tools(tools_schema: &Value) -> Value {
    let Some(arr) = tools_schema.as_array() else {
        return tools_schema.clone();
    };
    Value::Array(
        arr.iter()
            .map(|tool| {
                if tool.get("type").and_then(Value::as_str) == Some("function")
                    && tool.get("function").is_some()
                {
                    let f = &tool["function"];
                    json!({
                        "type": "function",
                        "name": f.get("name").cloned().unwrap_or(Value::String(String::new())),
                        "description": f.get("description").cloned().unwrap_or(Value::String(String::new())),
                        "parameters": f.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}}))
                    })
                } else {
                    tool.clone()
                }
            })
            .collect(),
    )
}

async fn collect_sse_text_with_events(
    res: reqwest::Response,
    on_line: impl Fn(&str),
) -> Result<String> {
    let mut stream = res.bytes_stream();
    let mut text = String::new();
    let mut pending = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = String::from_utf8_lossy(&chunk?).to_string();
        text.push_str(&chunk);
        pending.push_str(&chunk);
        while let Some(pos) = pending.find('\n') {
            let mut line = pending[..pos].to_string();
            if line.ends_with('\r') {
                line.pop();
            }
            on_line(&line);
            pending = pending[pos + 1..].to_string();
        }
    }
    if !pending.is_empty() {
        on_line(&pending);
    }
    Ok(text)
}

fn emit_openai_chat_sse_delta(
    line: &str,
    emit: &(dyn Fn(LlmStreamEvent) + Send + Sync),
    model: &str,
) {
    let Some(data) = line.strip_prefix("data: ") else {
        return;
    };
    if data == "[DONE]" {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return;
    };
    emit_usage_if_present("chat_completions", model, v.get("usage"), emit);
    let delta = &v["choices"][0]["delta"];
    if let Some(s) = delta.get("content").and_then(Value::as_str)
        && !s.is_empty()
    {
        emit(LlmStreamEvent::ContentDelta {
            content: s.to_string(),
        });
    }
    if let Some(s) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .or_else(|| delta.get("thinking"))
        .and_then(Value::as_str)
        && !s.is_empty()
    {
        emit(LlmStreamEvent::ThinkingDelta {
            content: s.to_string(),
        });
    }
}

fn emit_responses_sse_delta(
    line: &str,
    emit: &(dyn Fn(LlmStreamEvent) + Send + Sync),
    model: &str,
) {
    let Some(data) = line.strip_prefix("data: ") else {
        return;
    };
    if data == "[DONE]" {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return;
    };
    emit_usage_if_present(
        "responses",
        model,
        v.pointer("/response/usage").or_else(|| v.get("usage")),
        emit,
    );
    match v.get("type").and_then(Value::as_str).unwrap_or_default() {
        "response.output_text.delta" => {
            if let Some(delta) = v.get("delta").and_then(Value::as_str)
                && !delta.is_empty()
            {
                emit(LlmStreamEvent::ContentDelta {
                    content: delta.to_string(),
                });
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = v.get("delta").and_then(Value::as_str)
                && !delta.is_empty()
            {
                emit(LlmStreamEvent::ThinkingDelta {
                    content: delta.to_string(),
                });
            }
        }
        _ => {}
    }
}

fn emit_claude_sse_delta(line: &str, emit: &(dyn Fn(LlmStreamEvent) + Send + Sync), model: &str) {
    let Some(data) = line.strip_prefix("data: ") else {
        return;
    };
    if data == "[DONE]" {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return;
    };
    emit_usage_if_present(
        "messages",
        model,
        v.get("usage").or_else(|| v.pointer("/message/usage")),
        emit,
    );
    if v.get("type").and_then(Value::as_str) != Some("content_block_delta") {
        return;
    }
    let delta = v.get("delta").unwrap_or(&Value::Null);
    match delta
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "text_delta" => {
            if let Some(text) = delta.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                emit(LlmStreamEvent::ContentDelta {
                    content: text.to_string(),
                });
            }
        }
        "thinking_delta" => {
            if let Some(text) = delta.get("thinking").and_then(Value::as_str)
                && !text.is_empty()
            {
                emit(LlmStreamEvent::ThinkingDelta {
                    content: text.to_string(),
                });
            }
        }
        _ => {}
    }
}

fn emit_usage_if_present(
    api_mode: &str,
    model: &str,
    usage: Option<&Value>,
    emit: &(dyn Fn(LlmStreamEvent) + Send + Sync),
) {
    let Some(usage) = usage.filter(|usage| usage.is_object()) else {
        return;
    };
    emit(LlmStreamEvent::Usage {
        usage: llm_usage_summary(api_mode, model, usage),
    });
}

fn record_sse_usage(cfg: &AgentConfig, api_mode: &str, text: &str) -> Result<()> {
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let usage = if api_mode == "responses" {
            v.pointer("/response/usage")
                .or_else(|| v.get("usage"))
                .unwrap_or(&Value::Null)
        } else if api_mode == "messages" {
            v.get("usage")
                .or_else(|| v.pointer("/message/usage"))
                .unwrap_or(&Value::Null)
        } else {
            v.get("usage").unwrap_or(&Value::Null)
        };
        if usage.is_object() {
            record_usage(cfg, api_mode, usage)?;
        }
    }
    Ok(())
}

fn record_usage(cfg: &AgentConfig, api_mode: &str, usage: &Value) -> Result<()> {
    if !usage.is_object() {
        return Ok(());
    }
    let summary = usage_summary(api_mode, usage);
    tracing::info!("{}", summary);
    fs::create_dir_all(&cfg.logs_dir)?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.logs_dir.join("llm_usage.jsonl"))?;
    writeln!(
        f,
        "{}",
        serde_json::to_string(&json!({
            "ts": chrono::Local::now().to_rfc3339(),
            "api_mode": api_mode,
            "model": cfg.openai_model,
            "summary": summary,
            "usage": usage,
        }))?
    )?;
    Ok(())
}

fn llm_usage_summary(api_mode: &str, model: &str, usage: &Value) -> LlmUsageSummary {
    LlmUsageSummary {
        api_mode: api_mode.to_string(),
        model: model.to_string(),
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

fn usage_u64_any(value: &Value, pointers: &[&str]) -> Option<u64> {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_u64))
}

fn usage_summary(api_mode: &str, usage: &Value) -> String {
    match api_mode {
        "responses" => format!(
            "[Cache] input={} cached={}",
            usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        ),
        "messages" => format!(
            "[Cache] input={} creation={} read={}",
            usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            usage
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        ),
        _ => format!(
            "[Cache] input={} cached={}",
            usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        ),
    }
}

async fn response_json(res: reqwest::Response, status: StatusCode) -> Result<Value> {
    let text = res.text().await.with_context(|| {
        format!(
            "read LLM response body status {status}; request may have exceeded timeout while waiting for a long-context completion. Increase OPENAI_TIMEOUT_SECS/KODA_LLM_TIMEOUT_SECS or set OPENAI_STREAM=true"
        )
    })?;
    serde_json::from_str(&text).with_context(|| {
        let preview = smart_preview(&text, 500);
        format!("parse response status {status}; body preview: {preview}")
    })
}

fn smart_preview(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        format!(
            "{}...[truncated]",
            text.chars().take(max_chars).collect::<String>()
        )
    }
}

pub fn parse_openai_chat_json(body: &Value) -> Result<AgentResponse> {
    let msg = body
        .pointer("/choices/0/message")
        .context("missing choices[0].message")?;
    let content = msg
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let thinking = msg
        .get("reasoning_content")
        .or_else(|| msg.get("reasoning"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let tool_calls = msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let id = tc.get("id").and_then(Value::as_str).map(str::to_string);
                    let f = tc.get("function")?;
                    let name = f.get("name")?.as_str()?.to_string();
                    let args_raw = f.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                    let args = serde_json::from_str(args_raw)
                        .unwrap_or(Value::String(args_raw.to_string()));
                    Some(ToolCall { id, name, args })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(AgentResponse {
        thinking,
        content,
        tool_calls,
        raw: body.clone(),
    })
}

pub fn parse_openai_sse_lines(lines: &[&str]) -> Result<AgentResponse> {
    let mut content = String::new();
    let mut thinking = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for line in lines {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let v: Value = serde_json::from_str(data)?;
        let delta = &v["choices"][0]["delta"];
        if let Some(s) = delta.get("content").and_then(Value::as_str) {
            content.push_str(s);
        }
        if let Some(s) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
        {
            thinking.push_str(s);
        }
        if let Some(arr) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in arr {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while tool_calls.len() <= idx {
                    tool_calls.push(ToolCall {
                        id: None,
                        name: String::new(),
                        args: Value::String(String::new()),
                    });
                }
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    tool_calls[idx].id = Some(id.to_string());
                }
                if let Some(name) = tc.pointer("/function/name").and_then(Value::as_str) {
                    tool_calls[idx].name.push_str(name);
                }
                if let Some(args) = tc.pointer("/function/arguments").and_then(Value::as_str) {
                    let cur = tool_calls[idx]
                        .args
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                        + args;
                    tool_calls[idx].args = Value::String(cur);
                }
            }
        }
    }
    for tc in &mut tool_calls {
        if let Some(s) = tc.args.as_str() {
            tc.args = serde_json::from_str(s).unwrap_or(Value::String(s.to_string()));
        }
    }
    Ok(AgentResponse {
        thinking,
        content,
        tool_calls,
        raw: Value::Null,
    })
}

pub fn parse_responses_json(body: &Value) -> Result<AgentResponse> {
    let mut content = body
        .get("output_text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut thinking = String::new();
    let mut tool_calls = Vec::new();
    if let Some(output) = body.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                "message" => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(text) = part
                                .get("text")
                                .or_else(|| part.get("output_text"))
                                .and_then(Value::as_str)
                            {
                                content.push_str(text);
                            }
                        }
                    }
                }
                "function_call" => {
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let args_raw = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    let args =
                        serde_json::from_str(args_raw).unwrap_or(Value::String(args_raw.into()));
                    let id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    if !name.is_empty() {
                        tool_calls.push(ToolCall { id, name, args });
                    }
                }
                "reasoning" => {
                    if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                        for part in summary {
                            if let Some(text) = part
                                .get("text")
                                .or_else(|| part.get("summary_text"))
                                .and_then(Value::as_str)
                            {
                                thinking.push_str(text);
                            }
                        }
                    }
                    if let Some(text) = item.get("content").and_then(Value::as_str) {
                        thinking.push_str(text);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(AgentResponse {
        thinking,
        content,
        tool_calls,
        raw: body.clone(),
    })
}

pub fn parse_responses_sse_lines(lines: &[&str]) -> Result<AgentResponse> {
    let mut content = String::new();
    let mut thinking = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut arg_buffers: Vec<String> = Vec::new();
    for line in lines {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let v: Value = serde_json::from_str(data)?;
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "response.output_text.delta" => {
                if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                    content.push_str(delta);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                    thinking.push_str(delta);
                }
            }
            "response.function_call_arguments.delta" => {
                let idx = v.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while arg_buffers.len() <= idx {
                    arg_buffers.push(String::new());
                }
                if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                    arg_buffers[idx].push_str(delta);
                }
            }
            "response.output_item.done" | "response.output_item.added" => {
                if let Some(item) = v.get("item")
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
                {
                    let parsed = parse_responses_json(&json!({"output":[item]}))?;
                    for mut tc in parsed.tool_calls {
                        let idx =
                            v.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        if (tc.args == json!({}) || tc.args.as_str().is_some_and(str::is_empty))
                            && let Some(buf) = arg_buffers.get(idx)
                        {
                            tc.args =
                                serde_json::from_str(buf).unwrap_or(Value::String(buf.clone()));
                        }
                        tool_calls.push(tc);
                    }
                    thinking.push_str(&parsed.thinking);
                }
            }
            "response.completed" => {
                if let Some(response) = v.get("response") {
                    let parsed = parse_responses_json(response)?;
                    if content.is_empty() {
                        content = parsed.content;
                    }
                    if tool_calls.is_empty() {
                        tool_calls = parsed.tool_calls;
                    }
                    if thinking.is_empty() {
                        thinking = parsed.thinking;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(AgentResponse {
        thinking,
        content,
        tool_calls,
        raw: Value::Null,
    })
}

pub fn parse_claude_messages_json(body: &Value) -> Result<AgentResponse> {
    let mut content = String::new();
    let mut thinking = String::new();
    let mut tool_calls = Vec::new();
    for block in body
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    content.push_str(text);
                }
            }
            "thinking" => {
                if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                    thinking.push_str(text);
                }
            }
            "tool_use" => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !name.is_empty() {
                    tool_calls.push(ToolCall {
                        id: block.get("id").and_then(Value::as_str).map(str::to_string),
                        name,
                        args: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(AgentResponse {
        thinking,
        content,
        tool_calls,
        raw: body.clone(),
    })
}

pub fn parse_claude_sse_lines(lines: &[&str]) -> Result<AgentResponse> {
    let mut content = String::new();
    let mut thinking = String::new();
    let mut blocks: Vec<Value> = Vec::new();
    let mut partial_json: Vec<String> = Vec::new();
    for line in lines {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let v: Value = serde_json::from_str(data)?;
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "content_block_start" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while blocks.len() <= idx {
                    blocks.push(Value::Null);
                    partial_json.push(String::new());
                }
                blocks[idx] = v.get("content_block").cloned().unwrap_or(Value::Null);
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = v.get("delta").unwrap_or(&Value::Null);
                match delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text_delta" => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            content.push_str(text);
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            thinking.push_str(text);
                            if let Some(block) = blocks.get_mut(idx).and_then(Value::as_object_mut)
                            {
                                let cur = block
                                    .get("thinking")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                block.insert("thinking".into(), Value::String(cur + text));
                            }
                        }
                    }
                    "signature_delta" => {
                        if let Some(sig) = delta.get("signature").and_then(Value::as_str)
                            && let Some(block) = blocks.get_mut(idx).and_then(Value::as_object_mut)
                        {
                            let cur = block
                                .get("signature")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            block.insert("signature".into(), Value::String(cur + sig));
                        }
                    }
                    "input_json_delta" => {
                        while partial_json.len() <= idx {
                            partial_json.push(String::new());
                        }
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                            partial_json[idx].push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let mut tool_calls = Vec::new();
    for (idx, block) in blocks.iter().enumerate() {
        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
            let raw = partial_json.get(idx).cloned().unwrap_or_default();
            let args = serde_json::from_str(&raw)
                .unwrap_or_else(|_| block.get("input").cloned().unwrap_or_else(|| json!({})));
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !name.is_empty() {
                tool_calls.push(ToolCall {
                    id: block.get("id").and_then(Value::as_str).map(str::to_string),
                    name,
                    args,
                });
            }
        }
    }
    Ok(AgentResponse {
        thinking,
        content,
        tool_calls,
        raw: json!({"content": blocks}),
    })
}

#[derive(Clone)]
pub struct MockLlmClient {
    pub responses: Arc<Vec<AgentResponse>>,
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn chat(&self, messages: &[ChatMessage], _tools_schema: &Value) -> Result<AgentResponse> {
        let idx = messages.iter().filter(|m| m.role == "assistant").count();
        Ok(self
            .responses
            .get(idx)
            .cloned()
            .unwrap_or_else(|| AgentResponse {
                thinking: String::new(),
                content: "OK".into(),
                tool_calls: vec![],
                raw: Value::Null,
            }))
    }
    fn name(&self) -> String {
        "MockLLM".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn parses_json_tool_call() {
        let v = json!({"choices":[{"message":{"content":"", "reasoning_content":"think", "tool_calls":[{"id":"1","function":{"name":"file_read","arguments":"{\"path\":\"README.md\"}"}}]}}]});
        let r = parse_openai_chat_json(&v).unwrap();
        assert_eq!(r.thinking, "think");
        assert_eq!(r.tool_calls[0].name, "file_read");
        assert_eq!(r.tool_calls[0].args["path"], "README.md");
    }
    #[test]
    fn parses_sse_tool_call() {
        let lines = [
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"x\",\"function\":{\"name\":\"file_read\",\"arguments\":\"{\\\"path\\\":\\\"\"}}]}}]}",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"README.md\\\"}\"}}]}}]}",
            "data: [DONE]",
        ];
        let r = parse_openai_sse_lines(&lines).unwrap();
        assert_eq!(r.thinking, "think");
        assert_eq!(r.tool_calls[0].args["path"], "README.md");
    }

    #[test]
    fn parses_openai_sse_parallel_tool_calls_and_reasoning_alias() {
        let lines = [
            r#"data: {"choices":[{"delta":{"reasoning":"plan"}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"file_read","arguments":"{\"path\":\"a"}},{"index":1,"id":"call_b","function":{"name":"file_write","arguments":"{\"path\":\"b"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":".txt\"}"}},{"index":1,"function":{"arguments":"\",\"content\":\"ok\"}"}}]}}]}"#,
            "data: [DONE]",
        ];
        let r = parse_openai_sse_lines(&lines).unwrap();
        assert_eq!(r.thinking, "plan");
        assert_eq!(r.tool_calls.len(), 2);
        assert_eq!(r.tool_calls[0].id.as_deref(), Some("call_a"));
        assert_eq!(r.tool_calls[0].args["path"], "a.txt");
        assert_eq!(r.tool_calls[1].name, "file_write");
        assert_eq!(r.tool_calls[1].args["content"], "ok");
    }

    #[test]
    fn parses_responses_json_tool_call() {
        let v = json!({
            "output": [
                {"type":"message","content":[{"type":"output_text","text":"hi"}]},
                {"type":"function_call","call_id":"call_x","name":"file_read","arguments":"{\"path\":\"README.md\"}"}
            ]
        });
        let r = parse_responses_json(&v).unwrap();
        assert_eq!(r.content, "hi");
        assert_eq!(r.tool_calls[0].id.as_deref(), Some("call_x"));
        assert_eq!(r.tool_calls[0].args["path"], "README.md");
    }

    #[test]
    fn parses_responses_json_reasoning_and_multiple_function_calls() {
        let v = json!({
            "output_text":"prefix ",
            "output":[
                {"type":"reasoning","summary":[{"type":"summary_text","text":"why"}],"content":" detail"},
                {"type":"message","content":[{"type":"output_text","text":"body"}]},
                {"type":"function_call","id":"fc_1","name":"file_read","arguments":r#"{"path":"a.txt"}"#},
                {"type":"function_call","call_id":"call_2","name":"code_run","arguments":"not-json"}
            ]
        });
        let r = parse_responses_json(&v).unwrap();
        assert_eq!(r.content, "prefix body");
        assert_eq!(r.thinking, "why detail");
        assert_eq!(r.tool_calls.len(), 2);
        assert_eq!(r.tool_calls[0].id.as_deref(), Some("fc_1"));
        assert_eq!(r.tool_calls[0].args["path"], "a.txt");
        assert_eq!(r.tool_calls[1].args, Value::String("not-json".into()));
    }

    #[test]
    fn parses_responses_sse_completed() {
        let lines = [
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think\"}",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"path\\\":\\\"README.md\\\"}\"}",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"O\"}",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"K\"}",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_x\",\"name\":\"file_read\",\"arguments\":\"\"}}",
            "data: [DONE]",
        ];
        let r = parse_responses_sse_lines(&lines).unwrap();
        assert_eq!(r.thinking, "think");
        assert_eq!(r.content, "OK");
        assert_eq!(r.tool_calls[0].name, "file_read");
        assert_eq!(r.tool_calls[0].args["path"], "README.md");
    }

    #[test]
    fn retry_policy_matches_transient_failures() {
        assert!(should_retry_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(should_retry_status(StatusCode::BAD_GATEWAY));
        assert!(!should_retry_status(StatusCode::BAD_REQUEST));
        assert_eq!(retry_delay(1), Duration::from_millis(250));
        assert_eq!(retry_delay(3), Duration::from_millis(1000));
    }

    #[test]
    fn claude_thinking_options_match_upstream_payload_shape() {
        let mut cfg = cfg_with_models();
        cfg.thinking_type = Some("adaptive".into());
        let mut payload = json!({});
        apply_claude_options(&mut payload, &cfg);
        assert_eq!(payload["thinking"], json!({"type":"adaptive"}));

        cfg.thinking_type = Some("enabled".into());
        cfg.thinking_budget_tokens = Some(32768);
        let mut payload = json!({});
        apply_claude_options(&mut payload, &cfg);
        assert_eq!(
            payload["thinking"],
            json!({"type":"enabled","budget_tokens":32768})
        );

        cfg.thinking_budget_tokens = None;
        let mut payload = json!({});
        apply_claude_options(&mut payload, &cfg);
        assert!(payload.get("thinking").is_none());
    }

    #[test]
    fn session_overrides_update_llm_generation_options() {
        let mut cfg = cfg_with_models();
        cfg.temperature = Some(1.0);
        cfg.max_tokens = Some(8192);
        let mut overrides = serde_json::Map::new();
        overrides.insert("temperature".into(), json!(0.3));
        overrides.insert("max_tokens".into(), json!(2048));
        overrides.insert("reasoning_effort".into(), json!("HIGH"));
        overrides.insert("thinking_type".into(), json!("adaptive"));
        overrides.insert("thinking_budget_tokens".into(), json!(32768));
        overrides.insert("stream".into(), json!(true));
        apply_session_overrides_to_cfg(&mut cfg, &overrides);
        assert_eq!(cfg.temperature, Some(0.3));
        assert_eq!(cfg.max_tokens, Some(2048));
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(cfg.thinking_type.as_deref(), Some("adaptive"));
        assert_eq!(cfg.thinking_budget_tokens, Some(32768));
        assert!(cfg.stream);
        assert!(
            OpenAiClient::new(cfg)
                .set_session_option("max_tokens", &json!(1024))
                .unwrap()
        );
    }

    #[test]
    fn expands_internal_tool_messages_for_openai_chat() {
        let messages = vec![
            ChatMessage::text("system", "sys"),
            ChatMessage::text("user", "read"),
            ChatMessage {
                role: "assistant".into(),
                content: json!({"text":"","thinking":"必须读文件","tool_calls":[{"id":"call_1","name":"file_read","args":{"path":"a.txt"}}]}),
            },
            ChatMessage {
                role: "user".into(),
                content: json!({"content":"\n","tool_results":[{"tool_call_id":"call_1","name":"file_read","content":"1|alpha"}]}),
            },
        ];
        let out = openai_chat_messages(&messages);
        assert_eq!(out[2]["role"], "assistant");
        assert_eq!(out[2]["reasoning_content"], "必须读文件");
        assert_eq!(out[2]["tool_calls"][0]["function"]["name"], "file_read");
        assert_eq!(
            out[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a.txt\"}"
        );
        assert_eq!(out[3]["role"], "tool");
        assert_eq!(out[3]["tool_call_id"], "call_1");
        assert_eq!(out[3]["content"], "1|alpha");
    }

    fn cfg_with_models() -> AgentConfig {
        AgentConfig {
            home_dir: ".".into(),
            workspace_dir: ".".into(),
            resource_dir: ".".into(),
            root_dir: ".".into(),
            temp_dir: "temp".into(),
            memory_dir: "memory".into(),
            logs_dir: "logs".into(),
            sessions_dir: "sessions".into(),
            browser_dir: "browser".into(),
            openai_base_url: "http://primary".into(),
            openai_api_key: "sk-primary".into(),
            openai_model: "primary".into(),
            llm_api_style: "chat".into(),
            max_turns: 1,
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
            llm_configs: vec![
                LlmModelConfig {
                    name: "primary".into(),
                    base_url: "http://primary".into(),
                    api_key: "sk-primary".into(),
                    model: "primary".into(),
                    api_style: "chat".into(),
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
                },
                LlmModelConfig {
                    name: "backup".into(),
                    base_url: "http://backup".into(),
                    api_key: "sk-backup".into(),
                    model: "backup".into(),
                    api_style: "responses".into(),
                    stream: true,
                    timeout_secs: 900,
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
                },
            ],
        }
    }

    #[test]
    fn multi_llm_lists_and_switches_models() {
        let llm = MultiLlmClient::new(cfg_with_models());
        let list = llm.list_llms();
        assert_eq!(list.len(), 2);
        assert!(list[0].2);
        llm.switch_llm(1).unwrap();
        let list = llm.list_llms();
        assert!(list[1].2);
        assert!(llm.switch_llm(99).is_err());
    }

    #[tokio::test]
    async fn multi_llm_forwards_stream_events() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut req = [0_u8; 4096];
            let _ = socket.read(&mut req);
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"想\"}}],\"usage\":null}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}],\"usage\":null}\n\n",
                "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
                "data: [DONE]\n\n"
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(resp.as_bytes()).unwrap();
        });

        let mut cfg = cfg_with_models();
        cfg.openai_base_url = format!("http://{addr}/v1");
        cfg.stream = true;
        cfg.llm_configs = vec![LlmModelConfig {
            name: "local".into(),
            base_url: cfg.openai_base_url.clone(),
            api_key: "sk-local".into(),
            model: "local".into(),
            api_style: "chat".into(),
            stream: true,
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
            failover: true,
            custom_headers: Default::default(),
        }];
        let llm = MultiLlmClient::new(cfg);
        let events = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let events_for_emit = Arc::clone(&events);
        let resp =
            llm.chat_with_events(&[ChatMessage::text("user", "hi")], &json!([]), &|event| {
                match event {
                    LlmStreamEvent::ContentDelta { .. } => {
                        events_for_emit.lock().unwrap().push("content");
                    }
                    LlmStreamEvent::ThinkingDelta { .. } => {
                        events_for_emit.lock().unwrap().push("thinking");
                    }
                    LlmStreamEvent::Usage { .. } => {
                        events_for_emit.lock().unwrap().push("usage");
                    }
                }
            })
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(resp.thinking, "想");
        assert_eq!(resp.content, "好");
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &["thinking", "content", "usage"]
        );
    }

    #[test]
    fn openai_chat_synthesizes_missing_tool_results_to_avoid_400() {
        let messages = vec![
            ChatMessage::text("user", "ask user"),
            ChatMessage {
                role: "assistant".into(),
                content: json!({"text":"","tool_calls":[{"id":"call_ask","name":"ask_user","args":{"question":"continue?"}}]}),
            },
            ChatMessage::text("user", "yes"),
        ];
        let out = openai_chat_messages(&messages);
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[2]["role"], "tool");
        assert_eq!(out[2]["tool_call_id"], "call_ask");
        assert!(
            out[2]["content"]
                .as_str()
                .unwrap()
                .contains("tool result missing")
        );
        assert_eq!(out[3]["role"], "user");
    }

    #[test]
    fn expands_internal_tool_messages_for_responses_api() {
        let messages = vec![
            ChatMessage::text("system", "sys"),
            ChatMessage {
                role: "assistant".into(),
                content: json!({"text":"","tool_calls":[{"id":"call_1","name":"file_read","args":{"path":"a.txt"}}]}),
            },
            ChatMessage {
                role: "user".into(),
                content: json!({"content":"\n","tool_results":[{"tool_call_id":"call_1","name":"file_read","content":{"status":"success"}}]}),
            },
        ];
        let out = responses_input(&messages);
        assert_eq!(out[0]["role"], "developer");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[2]["type"], "function_call");
        assert_eq!(out[2]["call_id"], "call_1");
        assert_eq!(out[3]["type"], "function_call_output");
        assert_eq!(out[3]["output"], "{\"status\":\"success\"}");
    }

    #[test]
    fn expands_internal_messages_for_claude() {
        let messages = vec![
            ChatMessage::text("system", "sys"),
            ChatMessage::text("user", "read"),
            ChatMessage {
                role: "assistant".into(),
                content: json!({"text":"","thinking":"think","thinking_signature":"sig","tool_calls":[{"id":"toolu_1","name":"file_read","args":{"path":"a.txt"}}]}),
            },
            ChatMessage {
                role: "user".into(),
                content: json!({"content":"\n","tool_results":[{"tool_call_id":"toolu_1","name":"file_read","content":"1|alpha"}]}),
            },
        ];
        let (system, out) = claude_messages_input(&messages, "claude");
        assert_eq!(system, "sys");
        assert_eq!(out[1]["content"][0]["type"], "thinking");
        assert_eq!(out[1]["content"][0]["signature"], "sig");
        assert_eq!(out[1]["content"][1]["type"], "tool_use");
        assert_eq!(out[2]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn claude_drops_unsigned_thinking_and_preserves_signed_raw_blocks() {
        let messages = vec![
            ChatMessage::text("user", "read"),
            ChatMessage {
                role: "assistant".into(),
                content: json!({"text":"unsigned","thinking":"will drop"}),
            },
            ChatMessage {
                role: "assistant".into(),
                content: json!({"text":"signed","raw":{"content":[{"type":"thinking","thinking":"keep","signature":"sig2"},{"type":"text","text":"signed"}]}}),
            },
        ];
        let (_, out) = claude_messages_input(&messages, "claude");
        assert_eq!(out[1]["content"][0]["type"], "text");
        assert_eq!(out[2]["content"][0]["signature"], "sig2");
    }

    #[test]
    fn parses_text_protocol_tool_calls() {
        let mut response = AgentResponse {
            thinking: String::new(),
            content: "<thinking>need read</thinking>\n<summary>读文件</summary>\n<tool_use>{\"name\":\"file_read\",\"arguments\":{\"path\":\"README.md\"}}</tool_use>".into(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        response = parse_text_protocol_response(response);
        assert_eq!(response.thinking, "need read");
        assert!(response.content.contains("<summary>读文件</summary>"));
        assert_eq!(response.tool_calls[0].name, "file_read");
        assert_eq!(response.tool_calls[0].args["path"], "README.md");
    }

    #[test]
    fn text_protocol_parses_multiple_fenced_and_param_alias_calls() {
        let mut response = AgentResponse {
            thinking: String::new(),
            content: r#"<tool_use>```json
{"id":"a","tool":"file_read","params":{"path":"a.txt"}}
```</tool_use>
<tool_call>{"function":"code_run","input":{"type":"bash","code":"echo ok"}}</tool_call>done"#
                .into(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        response = parse_text_protocol_response(response);
        assert_eq!(response.content, "done");
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id.as_deref(), Some("a"));
        assert_eq!(response.tool_calls[0].args["path"], "a.txt");
        assert_eq!(response.tool_calls[1].name, "code_run");
        assert_eq!(response.tool_calls[1].args["code"], "echo ok");
    }

    #[test]
    fn text_protocol_extracts_bare_json_tool_object() {
        let mut response = AgentResponse {
            thinking: String::new(),
            content: r#"先读 {"name":"file_read","args":{"path":"README.md"}} 后继续"#.into(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        response = parse_text_protocol_response(response);
        assert_eq!(response.tool_calls[0].name, "file_read");
        assert_eq!(response.tool_calls[0].args["path"], "README.md");
        assert_eq!(response.content.trim(), "先读  后继续");
    }

    #[test]
    fn text_protocol_bad_json_emits_bad_json_tool() {
        let mut response = AgentResponse {
            thinking: String::new(),
            content:
                "<summary>bad</summary><tool_use>{\"name\":\"file_read\",\"arguments\":</tool_use>"
                    .into(),
            tool_calls: vec![],
            raw: Value::Null,
        };
        response = parse_text_protocol_response(response);
        assert_eq!(response.tool_calls[0].name, "bad_json");
        assert!(
            response.tool_calls[0].args["msg"]
                .as_str()
                .unwrap()
                .contains("Failed to parse")
        );
    }

    #[test]
    fn cache_markers_and_usage_summary_match_upstream_shape() {
        let mut messages = vec![
            json!({"role":"user","content":"one"}),
            json!({"role":"assistant","content":"two"}),
            json!({"role":"user","content":"three"}),
        ];
        stamp_oai_cache_markers(&mut messages, "claude-3-5");
        assert_eq!(
            messages[0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(
            messages[2]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(
            usage_summary(
                "chat_completions",
                &json!({"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":7}})
            ),
            "[Cache] input=10 cached=7"
        );
        assert_eq!(
            usage_summary(
                "messages",
                &json!({"input_tokens":10,"cache_creation_input_tokens":2,"cache_read_input_tokens":3})
            ),
            "[Cache] input=10 creation=2 read=3"
        );
    }

    #[test]
    fn claude_one_m_context_suffix_maps_to_beta_header() {
        let (model, beta) = claude_model_and_beta("claude-opus-4-7[1m]");
        assert_eq!(model, "claude-opus-4-7");
        assert!(beta.contains("prompt-caching-2024-07-31"));
        assert!(beta.contains("context-1m-2025-08-07"));
    }

    #[test]
    fn builds_text_protocol_prompt_with_tool_results() {
        let messages = vec![
            ChatMessage::text("system", "sys"),
            ChatMessage {
                role: "user".into(),
                content: json!({"content":"continue","tool_results":[{"tool_call_id":"call_1","name":"file_read","content":"1|alpha"}]}),
            },
        ];
        let prompt = text_protocol_messages(&messages, &json!([]));
        assert!(prompt[0]["content"].as_str().unwrap().contains("交互协议"));
        assert!(
            prompt[1]["content"]
                .as_str()
                .unwrap()
                .contains("<tool_result>1|alpha</tool_result>")
        );
    }

    #[test]
    fn text_protocol_reuses_tool_instruction_after_first_call() {
        let client = OpenAiClient::new(cfg_with_models());
        let messages = vec![ChatMessage::text("user", "hello")];
        let schema = json!([{"type":"function","function":{"name":"file_read","parameters":{"type":"object"}}}]);
        let first = client.text_protocol_messages(&messages, &schema);
        let second = client.text_protocol_messages(&messages, &schema);
        assert!(
            first[0]["content"]
                .as_str()
                .unwrap()
                .contains("Tools (mounted")
        );
        assert!(
            second[0]["content"]
                .as_str()
                .unwrap()
                .contains("工具库状态")
        );
    }

    #[test]
    fn history_trimming_compresses_old_tool_tags_and_sanitizes_leading_user() {
        let big = "x".repeat(7000);
        let mut messages = vec![ChatMessage::text("system", "sys")];
        for i in 0..20 {
            messages.push(ChatMessage {
                role: "user".into(),
                content: json!({"content":format!("u{i} <history>{big}</history>"),"tool_results":[{"tool_call_id":"old","name":"file_read","content":big}]}),
            });
            messages.push(ChatMessage {
                role: "assistant".into(),
                content: json!({"text":format!("<thinking>{big}</thinking>"),"tool_calls":[]}),
            });
        }
        let trimmed = trim_messages_history(&messages);
        assert!(trimmed.len() < messages.len());
        assert_eq!(trimmed[0].role, "system");
        let first_user = trimmed.iter().find(|m| m.role == "user").unwrap();
        assert!(
            first_user
                .content
                .get("tool_results")
                .and_then(Value::as_array)
                .is_none()
        );
        let encoded = serde_json::to_string(&trimmed).unwrap();
        assert!(encoded.contains("[Truncated]") || encoded.contains("[...]"));
    }

    #[test]
    fn parses_claude_messages_json_and_sse() {
        let v = json!({"content":[
            {"type":"thinking","thinking":"think"},
            {"type":"text","text":"hi"},
            {"type":"tool_use","id":"toolu_1","name":"file_read","input":{"path":"README.md"}}
        ]});
        let r = parse_claude_messages_json(&v).unwrap();
        assert_eq!(r.thinking, "think");
        assert_eq!(r.content, "hi");
        assert_eq!(r.tool_calls[0].args["path"], "README.md");

        let lines = [
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"file_read\",\"input\":{}}}",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}",
        ];
        let r = parse_claude_sse_lines(&lines).unwrap();
        assert_eq!(r.content, "ok");
        assert_eq!(r.tool_calls[0].id.as_deref(), Some("toolu_1"));
        assert_eq!(r.tool_calls[0].args["path"], "README.md");

        let lines = [
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"plan\"}}",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}",
        ];
        let r = parse_claude_sse_lines(&lines).unwrap();
        assert_eq!(r.thinking, "plan");
        assert_eq!(r.raw["content"][0]["signature"], "sig");
    }

    #[test]
    fn parses_claude_sse_multiple_tool_blocks_with_fallback_input() {
        let lines = [
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_a\",\"name\":\"file_read\",\"input\":{}}}",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_b\",\"name\":\"file_write\",\"input\":{\"path\":\"fallback.txt\"}}}",
        ];
        let r = parse_claude_sse_lines(&lines).unwrap();
        assert_eq!(r.tool_calls.len(), 2);
        assert_eq!(r.tool_calls[0].args["path"], "a.txt");
        assert_eq!(r.tool_calls[1].args["path"], "fallback.txt");
    }
}
