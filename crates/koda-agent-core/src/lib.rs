pub mod python_runtime;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Local;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    pub content: Value,
}

impl ChatMessage {
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Value::String(text.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResult {
    pub tool_call_id: Option<String>,
    pub name: String,
    pub content: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentResponse {
    #[serde(default)]
    pub thinking: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StepOutcome {
    pub data: Value,
    pub next_prompt: Option<String>,
    pub should_exit: bool,
}

impl StepOutcome {
    pub fn done(data: impl Into<Value>) -> Self {
        Self {
            data: data.into(),
            next_prompt: None,
            should_exit: false,
        }
    }
    pub fn next(data: impl Into<Value>, prompt: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            next_prompt: Some(prompt.into()),
            should_exit: false,
        }
    }
    pub fn exit(data: impl Into<Value>) -> Self {
        Self {
            data: data.into(),
            next_prompt: None,
            should_exit: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    SlashOutput {
        content: String,
    },
    TurnStarted {
        turn: usize,
    },
    AssistantMessage {
        turn: usize,
        content: String,
    },
    AssistantMessageDelta {
        turn: usize,
        content: String,
    },
    ThinkingMessage {
        turn: usize,
        content: String,
    },
    ThinkingMessageDelta {
        turn: usize,
        content: String,
    },
    ToolStarted {
        turn: usize,
        index: usize,
        name: String,
        args: Value,
    },
    ToolFinished {
        turn: usize,
        index: usize,
        name: String,
        data: Value,
    },
    TurnFinished {
        turn: usize,
        stop_reason: String,
    },
    LlmUsage {
        turn: usize,
        usage: LlmUsageSummary,
    },
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmStreamEvent {
    ContentDelta { content: String },
    ThinkingDelta { content: String },
    Usage { usage: LlmUsageSummary },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct LlmUsageSummary {
    pub api_mode: String,
    pub model: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentPaths {
    pub home_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub resource_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub memory_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub browser_dir: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct AgentPathOptions {
    pub home_dir: Option<PathBuf>,
    pub workspace_dir: Option<PathBuf>,
    pub resource_dir: Option<PathBuf>,
    pub executable_dir: Option<PathBuf>,
}

pub fn default_koda_home() -> PathBuf {
    env::var_os("KODA_AGENT_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|p| p.join(".koda-agent")))
        .unwrap_or_else(|| PathBuf::from(".koda-agent"))
}

pub fn default_koda_config_dir() -> Option<PathBuf> {
    env::var_os("KODA_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|p| p.join("koda-agent")))
}

pub fn resolve_agent_paths(current_dir: impl Into<PathBuf>) -> AgentPaths {
    resolve_agent_paths_with_options(current_dir, AgentPathOptions::default())
}

pub fn resolve_agent_paths_with_options(
    current_dir: impl Into<PathBuf>,
    options: AgentPathOptions,
) -> AgentPaths {
    let current_dir = current_dir.into();
    let home_dir = options
        .home_dir
        .or_else(|| env::var_os("KODA_AGENT_HOME").map(PathBuf::from))
        .unwrap_or_else(default_koda_home);
    let workspace_dir = options
        .workspace_dir
        .or_else(|| env::var_os("KODA_WORKSPACE").map(PathBuf::from))
        .unwrap_or(current_dir.clone());
    let resource_dir = options
        .resource_dir
        .or_else(|| env::var_os("KODA_RESOURCE_DIR").map(PathBuf::from))
        .or_else(|| packaged_resource_dir(options.executable_dir.as_deref()))
        .or_else(|| source_resource_dir(&current_dir))
        .unwrap_or_else(|| home_dir.join("resources"));

    AgentPaths {
        temp_dir: home_dir.join("temp"),
        memory_dir: home_dir.join("memory"),
        logs_dir: home_dir.join("logs"),
        sessions_dir: home_dir.join("sessions"),
        browser_dir: home_dir.join("browser"),
        home_dir,
        workspace_dir,
        resource_dir,
    }
}

fn packaged_resource_dir(executable_dir: Option<&Path>) -> Option<PathBuf> {
    executable_dir
        .map(|dir| dir.join("resources"))
        .filter(|dir| has_resource_markers(dir))
}

fn source_resource_dir(current_dir: &Path) -> Option<PathBuf> {
    current_dir
        .ancestors()
        .find(|dir| has_source_resource_markers(dir))
        .map(Path::to_path_buf)
}

fn has_resource_markers(dir: &Path) -> bool {
    dir.join("assets/tools_schema.json").is_file() && dir.join("assets/sys_prompt.txt").is_file()
}

fn has_source_resource_markers(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file() && has_resource_markers(dir)
}

fn config_search_roots(current_dir: &Path, paths: &AgentPaths) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in [
        current_dir,
        paths.workspace_dir.as_path(),
        paths.home_dir.as_path(),
        paths.resource_dir.as_path(),
    ] {
        if !roots.iter().any(|existing| existing == root) {
            roots.push(root.to_path_buf());
        }
    }
    if let Some(config_dir) = default_koda_config_dir()
        && !roots.iter().any(|existing| existing == &config_dir)
    {
        roots.push(config_dir);
    }
    roots
}

fn load_dotenv_files(roots: &[PathBuf]) {
    for root in roots {
        let _ = dotenvy::from_path(root.join(".env"));
    }
}

fn copy_dir_missing(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_missing(&src_path, &dst_path)?;
        } else if file_type.is_file() && !dst_path.exists() {
            fs::copy(&src_path, &dst_path).with_context(|| {
                format!("copy {} to {}", src_path.display(), dst_path.display())
            })?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub home_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub resource_dir: PathBuf,
    pub root_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub memory_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub browser_dir: PathBuf,
    pub openai_base_url: String,
    pub openai_api_key: String,
    pub openai_model: String,
    pub llm_api_style: String,
    pub auth_scheme: Option<String>,
    pub auth_header: Option<String>,
    pub max_turns: usize,
    pub verbose: bool,
    pub stream: bool,
    pub timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub verify_tls: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub reasoning_effort: Option<String>,
    pub thinking_type: Option<String>,
    pub thinking_budget_tokens: Option<u64>,
    pub service_tier: Option<String>,
    pub proxy: Option<String>,
    pub failover: bool,
    pub custom_headers: BTreeMap<String, String>,
    pub mixin: MixinConfig,
    pub llm_configs: Vec<LlmModelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MixinConfig {
    pub llm_nos: Vec<String>,
    pub max_retries: usize,
    pub base_delay_secs: f64,
    pub spring_back_secs: u64,
}

impl Default for MixinConfig {
    fn default() -> Self {
        Self {
            llm_nos: Vec::new(),
            max_retries: 3,
            base_delay_secs: 1.5,
            spring_back_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmModelConfig {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub api_style: String,
    pub auth_scheme: Option<String>,
    pub auth_header: Option<String>,
    pub stream: bool,
    pub timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub verify_tls: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub reasoning_effort: Option<String>,
    pub thinking_type: Option<String>,
    pub thinking_budget_tokens: Option<u64>,
    pub service_tier: Option<String>,
    pub proxy: Option<String>,
    pub failover: bool,
    pub custom_headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct LlmToml {
    selector: Option<LlmSelectorToml>,
    defaults: Option<LlmEntry>,
    profiles: Option<Vec<LlmEntry>>,
    default: Option<LlmEntry>,
    mixin: Option<MixinToml>,
    models: Option<Vec<LlmEntry>>,
    llms: Option<Vec<LlmEntry>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct LlmSelectorToml {
    default: Option<String>,
    default_profile: Option<String>,
    default_model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct MixinToml {
    llm_nos: Option<Vec<toml::Value>>,
    max_retries: Option<usize>,
    base_delay: Option<f64>,
    base_delay_secs: Option<f64>,
    spring_back: Option<u64>,
    spring_back_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct LlmEntry {
    name: Option<String>,
    kind: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    api_key_env: Option<String>,
    auth_scheme: Option<String>,
    auth_header: Option<String>,
    api_key_header: Option<String>,
    model: Option<String>,
    models: Option<Vec<LlmModelEntry>>,
    api_style: Option<String>,
    api_mode: Option<String>,
    max_turns: Option<usize>,
    stream: Option<bool>,
    timeout_secs: Option<u64>,
    connect_timeout_secs: Option<u64>,
    connect_timeout: Option<u64>,
    verify_tls: Option<bool>,
    verify: Option<bool>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    reasoning_effort: Option<String>,
    thinking_type: Option<String>,
    thinking_budget_tokens: Option<u64>,
    service_tier: Option<String>,
    proxy: Option<String>,
    failover: Option<bool>,
    headers: Option<BTreeMap<String, String>>,
    custom_headers: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct LlmModelEntry {
    name: Option<String>,
    id: Option<String>,
    model: Option<String>,
    stream: Option<bool>,
    timeout_secs: Option<u64>,
    connect_timeout_secs: Option<u64>,
    connect_timeout: Option<u64>,
    verify_tls: Option<bool>,
    verify: Option<bool>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    reasoning_effort: Option<String>,
    thinking_type: Option<String>,
    thinking_budget_tokens: Option<u64>,
    service_tier: Option<String>,
    proxy: Option<String>,
    failover: Option<bool>,
    headers: Option<BTreeMap<String, String>>,
    custom_headers: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default)]
struct LegacyMykeyConfig {
    default: Option<LlmEntry>,
    models: Vec<LlmEntry>,
    mixin: Option<MixinConfig>,
}

impl AgentConfig {
    pub fn from_env(root_dir: impl Into<PathBuf>) -> Result<Self> {
        Self::from_env_with_path_options(root_dir, AgentPathOptions::default())
    }

    pub fn from_env_with_path_options(
        current_dir: impl Into<PathBuf>,
        options: AgentPathOptions,
    ) -> Result<Self> {
        let current_dir = current_dir.into();
        let paths = resolve_agent_paths_with_options(&current_dir, options);
        let config_roots = config_search_roots(&current_dir, &paths);
        load_dotenv_files(&config_roots);
        let toml_root = config_roots
            .iter()
            .find(|dir| dir.join("config/llms.toml").is_file())
            .cloned();
        let toml_doc = toml_root
            .as_ref()
            .and_then(|root| fs::read_to_string(root.join("config/llms.toml")).ok())
            .and_then(|s| toml::from_str::<LlmToml>(&s).ok());
        if let Some(doc) = &toml_doc
            && doc.profiles.as_ref().is_some_and(|p| !p.is_empty())
        {
            return Self::from_profile_toml(paths, toml_root.as_deref(), doc);
        }
        let toml_cfg = toml_doc.as_ref().and_then(|c| c.default.clone());
        let legacy_root = config_roots
            .iter()
            .find(|dir| dir.join("mykey.json").is_file() || dir.join("mykey.py").is_file())
            .cloned();
        let legacy_cfg = legacy_root
            .as_deref()
            .map(load_legacy_mykey_config)
            .unwrap_or_default();
        if toml_cfg.is_none() && legacy_cfg.default.is_none() {
            let has_legacy_env = ["OPENAI_BASE_URL", "OPENAI_API_KEY", "OPENAI_MODEL"]
                .iter()
                .any(|key| env::var(key).is_ok_and(|value| !value.trim().is_empty()));
            if has_legacy_env {
                bail!(
                    "legacy OPENAI_* environment detected without config/llms.toml; run `koda-agent config migrate` or `koda-agent config setup mimo`"
                );
            }
            bail!("LLM config missing; run `koda-agent config setup mimo`");
        }
        let openai_base_url = env::var("OPENAI_BASE_URL")
            .ok()
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.base_url.clone()))
            .or_else(|| legacy_cfg.default.as_ref().and_then(|c| c.base_url.clone()))
            .context("base_url missing in LLM config; run `koda-agent config setup mimo`")?;
        let openai_api_key = env::var("OPENAI_API_KEY")
            .ok()
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.api_key.clone()))
            .or_else(|| legacy_cfg.default.as_ref().and_then(|c| c.api_key.clone()))
            .context("api_key missing in LLM config; run `koda-agent config secret <ENV_NAME>`")?;
        let openai_model = env::var("OPENAI_MODEL")
            .ok()
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.model.clone()))
            .or_else(|| legacy_cfg.default.as_ref().and_then(|c| c.model.clone()))
            .context("model missing in LLM config; run `koda-agent config setup mimo`")?;
        let llm_api_style = env::var("OPENAI_API_STYLE")
            .or_else(|_| env::var("KODA_LLM_API_STYLE"))
            .ok()
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.api_style.clone()))
            .or_else(|| {
                legacy_cfg
                    .default
                    .as_ref()
                    .and_then(|c| c.api_style.clone())
            })
            .unwrap_or_else(|| "chat".into());
        let max_turns = env::var("KODA_MAX_TURNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.max_turns))
            .or_else(|| legacy_cfg.default.as_ref().and_then(|c| c.max_turns))
            .unwrap_or(70);
        let stream = env::var("OPENAI_STREAM")
            .ok()
            .and_then(|v| parse_bool(&v))
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.stream))
            .or_else(|| legacy_cfg.default.as_ref().and_then(|c| c.stream))
            .unwrap_or(false);
        let timeout_secs = env::var("OPENAI_TIMEOUT_SECS")
            .or_else(|_| env::var("KODA_LLM_TIMEOUT_SECS"))
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.timeout_secs))
            .or_else(|| legacy_cfg.default.as_ref().and_then(|c| c.timeout_secs))
            .unwrap_or(600);
        let connect_timeout_secs = env::var("OPENAI_CONNECT_TIMEOUT_SECS")
            .or_else(|_| env::var("KODA_LLM_CONNECT_TIMEOUT_SECS"))
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| toml_cfg.as_ref().and_then(entry_connect_timeout))
            .or_else(|| legacy_cfg.default.as_ref().and_then(entry_connect_timeout))
            .unwrap_or(30)
            .max(1);
        let verify_tls = env::var("OPENAI_VERIFY_TLS")
            .or_else(|_| env::var("KODA_LLM_VERIFY_TLS"))
            .ok()
            .and_then(|v| parse_bool(&v))
            .or_else(|| toml_cfg.as_ref().and_then(entry_verify_tls))
            .or_else(|| legacy_cfg.default.as_ref().and_then(entry_verify_tls))
            .unwrap_or(true);
        let temperature = env::var("OPENAI_TEMPERATURE")
            .or_else(|_| env::var("KODA_LLM_TEMPERATURE"))
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.temperature))
            .or_else(|| legacy_cfg.default.as_ref().and_then(|c| c.temperature));
        let max_tokens = env::var("OPENAI_MAX_TOKENS")
            .or_else(|_| env::var("KODA_LLM_MAX_TOKENS"))
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.max_tokens))
            .or_else(|| legacy_cfg.default.as_ref().and_then(|c| c.max_tokens));
        let reasoning_effort = env::var("OPENAI_REASONING_EFFORT")
            .or_else(|_| env::var("KODA_REASONING_EFFORT"))
            .ok()
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.reasoning_effort.clone()))
            .or_else(|| {
                legacy_cfg
                    .default
                    .as_ref()
                    .and_then(|c| c.reasoning_effort.clone())
            })
            .and_then(valid_reasoning_effort);
        let thinking_type = env::var("ANTHROPIC_THINKING_TYPE")
            .or_else(|_| env::var("KODA_THINKING_TYPE"))
            .ok()
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.thinking_type.clone()))
            .or_else(|| {
                legacy_cfg
                    .default
                    .as_ref()
                    .and_then(|c| c.thinking_type.clone())
            })
            .and_then(valid_thinking_type);
        let thinking_budget_tokens = env::var("ANTHROPIC_THINKING_BUDGET_TOKENS")
            .or_else(|_| env::var("KODA_THINKING_BUDGET_TOKENS"))
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.thinking_budget_tokens))
            .or_else(|| {
                legacy_cfg
                    .default
                    .as_ref()
                    .and_then(|c| c.thinking_budget_tokens)
            });
        let service_tier = env::var("OPENAI_SERVICE_TIER")
            .or_else(|_| env::var("KODA_SERVICE_TIER"))
            .ok()
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.service_tier.clone()))
            .or_else(|| {
                legacy_cfg
                    .default
                    .as_ref()
                    .and_then(|c| c.service_tier.clone())
            })
            .and_then(valid_service_tier);
        let proxy = env::var("OPENAI_PROXY")
            .or_else(|_| env::var("KODA_LLM_PROXY"))
            .ok()
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.proxy.clone()))
            .or_else(|| legacy_cfg.default.as_ref().and_then(|c| c.proxy.clone()));
        let failover = env::var("KODA_LLM_FAILOVER")
            .ok()
            .and_then(|v| parse_bool(&v))
            .or_else(|| toml_cfg.as_ref().and_then(|c| c.failover))
            .or_else(|| legacy_cfg.default.as_ref().and_then(|c| c.failover))
            .unwrap_or(true);
        let custom_headers = toml_cfg
            .as_ref()
            .and_then(entry_headers)
            .or_else(|| legacy_cfg.default.as_ref().and_then(entry_headers))
            .unwrap_or_default();
        let auth_scheme = toml_cfg
            .as_ref()
            .and_then(entry_auth_scheme)
            .or_else(|| legacy_cfg.default.as_ref().and_then(entry_auth_scheme));
        let auth_header = toml_cfg
            .as_ref()
            .and_then(entry_auth_header)
            .or_else(|| legacy_cfg.default.as_ref().and_then(entry_auth_header));
        let mixin = toml_root
            .as_deref()
            .and_then(load_mixin_config)
            .or(legacy_cfg.mixin)
            .unwrap_or_default();
        let primary_llm = LlmModelConfig {
            name: env::var("OPENAI_NAME")
                .ok()
                .or_else(|| toml_cfg.as_ref().and_then(|c| c.name.clone()))
                .unwrap_or_else(|| openai_model.clone()),
            base_url: openai_base_url.clone(),
            api_key: openai_api_key.clone(),
            model: openai_model.clone(),
            api_style: llm_api_style.clone(),
            auth_scheme: auth_scheme.clone(),
            auth_header: auth_header.clone(),
            stream,
            timeout_secs,
            connect_timeout_secs,
            verify_tls,
            temperature,
            max_tokens,
            reasoning_effort: reasoning_effort.clone(),
            thinking_type: thinking_type.clone(),
            thinking_budget_tokens,
            service_tier: service_tier.clone(),
            proxy: proxy.clone(),
            failover,
            custom_headers: custom_headers.clone(),
        };
        let llm_configs = load_llm_model_configs(
            toml_root.as_deref().unwrap_or(&paths.resource_dir),
            &primary_llm,
            toml_cfg.as_ref(),
            &legacy_cfg.models,
        );
        Ok(Self {
            home_dir: paths.home_dir,
            workspace_dir: paths.workspace_dir,
            root_dir: paths.resource_dir.clone(),
            resource_dir: paths.resource_dir,
            temp_dir: paths.temp_dir,
            memory_dir: paths.memory_dir,
            logs_dir: paths.logs_dir,
            sessions_dir: paths.sessions_dir,
            browser_dir: paths.browser_dir,
            openai_base_url,
            openai_api_key,
            openai_model,
            llm_api_style,
            auth_scheme,
            auth_header,
            max_turns,
            verbose: true,
            stream,
            timeout_secs,
            connect_timeout_secs,
            verify_tls,
            temperature,
            max_tokens,
            reasoning_effort,
            thinking_type,
            thinking_budget_tokens,
            service_tier,
            proxy,
            failover,
            custom_headers,
            mixin,
            llm_configs,
        })
    }

    fn from_profile_toml(
        paths: AgentPaths,
        toml_root: Option<&Path>,
        doc: &LlmToml,
    ) -> Result<Self> {
        let profiles = doc
            .profiles
            .as_ref()
            .context("llms.toml profiles missing")?;
        let env_profile = env::var("KODA_LLM_PROFILE")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let selector_profile = doc
            .selector
            .as_ref()
            .and_then(|s| s.default_profile.clone())
            .or_else(|| doc.selector.as_ref().and_then(|s| s.default.clone()))
            .filter(|v| !v.trim().is_empty());
        let selected_profile = env_profile
            .clone()
            .or_else(|| selector_profile.clone())
            .or_else(|| profiles.first().and_then(|p| p.name.clone()))
            .context("no LLM profile selected; run `koda-agent config setup mimo`")?;
        let selected_idx = profiles
            .iter()
            .position(|p| p.name.as_deref() == Some(selected_profile.as_str()))
            .with_context(|| format!("active LLM profile `{selected_profile}` does not exist"))?;
        let env_model = env::var("KODA_LLM_MODEL")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let selector_model = doc
            .selector
            .as_ref()
            .and_then(|s| s.default_model.clone())
            .filter(|v| !v.trim().is_empty());
        let first_profile_model = || {
            profiles[selected_idx]
                .models
                .as_ref()
                .and_then(|models| models.first())
                .and_then(|m| m.name.clone())
        };
        let selector_model_applies =
            env_profile.is_none() || selector_profile.as_deref() == Some(selected_profile.as_str());
        let selected_model = env_model
            .or_else(|| {
                selector_model_applies
                    .then(|| selector_model.clone())
                    .flatten()
            })
            .or_else(first_profile_model)
            .context("no LLM model selected; add [[profiles.models]] to llms.toml")?;
        let defaults = doc.defaults.as_ref();
        let mut llm_configs = Vec::new();
        let primary = profile_to_model_config(&profiles[selected_idx], &selected_model, defaults)?;
        llm_configs.push(primary.clone());
        for (idx, profile) in profiles.iter().enumerate() {
            let models = profile.models.as_ref().with_context(|| {
                let name = profile.name.as_deref().unwrap_or("<unnamed>");
                if profile.model.is_some() {
                    format!(
                        "profile `{name}` uses old `model` field; rewrite it as [[profiles.models]]"
                    )
                } else {
                    format!("profile `{name}` has no [[profiles.models]] entries")
                }
            })?;
            for model in models {
                let alias = model.name.as_deref().with_context(|| {
                    let name = profile.name.as_deref().unwrap_or("<unnamed>");
                    format!("profile `{name}` has a model without `name`")
                })?;
                if idx == selected_idx && alias == selected_model {
                    continue;
                }
                match profile_to_model_config(profile, alias, defaults) {
                    Ok(config) => llm_configs.push(config),
                    Err(e) => eprintln!("warning: skipping fallback profile model: {e}"),
                }
            }
        }
        let mixin = toml_root.and_then(load_mixin_config).unwrap_or_default();
        Ok(Self {
            home_dir: paths.home_dir,
            workspace_dir: paths.workspace_dir,
            root_dir: paths.resource_dir.clone(),
            resource_dir: paths.resource_dir,
            temp_dir: paths.temp_dir,
            memory_dir: paths.memory_dir,
            logs_dir: paths.logs_dir,
            sessions_dir: paths.sessions_dir,
            browser_dir: paths.browser_dir,
            openai_base_url: primary.base_url.clone(),
            openai_api_key: primary.api_key.clone(),
            openai_model: primary.model.clone(),
            llm_api_style: primary.api_style.clone(),
            auth_scheme: primary.auth_scheme.clone(),
            auth_header: primary.auth_header.clone(),
            max_turns: env::var("KODA_MAX_TURNS")
                .ok()
                .and_then(|v| v.parse().ok())
                .or_else(|| defaults.and_then(|d| d.max_turns))
                .unwrap_or(70),
            verbose: true,
            stream: primary.stream,
            timeout_secs: primary.timeout_secs,
            connect_timeout_secs: primary.connect_timeout_secs,
            verify_tls: primary.verify_tls,
            temperature: primary.temperature,
            max_tokens: primary.max_tokens,
            reasoning_effort: primary.reasoning_effort.clone(),
            thinking_type: primary.thinking_type.clone(),
            thinking_budget_tokens: primary.thinking_budget_tokens,
            service_tier: primary.service_tier.clone(),
            proxy: primary.proxy.clone(),
            failover: primary.failover,
            custom_headers: primary.custom_headers.clone(),
            mixin,
            llm_configs,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.home_dir)?;
        fs::create_dir_all(&self.temp_dir)?;
        fs::create_dir_all(&self.memory_dir)?;
        fs::create_dir_all(&self.logs_dir)?;
        fs::create_dir_all(&self.sessions_dir)?;
        fs::create_dir_all(&self.browser_dir)?;
        if !self.memory_dir.join("global_mem.txt").exists() {
            fs::write(
                self.memory_dir.join("global_mem.txt"),
                "# [Global Memory - L2]\n",
            )?;
        }
        if !self.memory_dir.join("global_mem_insight.txt").exists() {
            let template = self
                .resource_dir
                .join("assets/global_mem_insight_template.txt");
            let content = fs::read_to_string(template).unwrap_or_default();
            fs::write(self.memory_dir.join("global_mem_insight.txt"), content)?;
        }
        self.ensure_cdp_bridge_config()?;
        Ok(())
    }

    fn ensure_cdp_bridge_config(&self) -> Result<()> {
        let src_bridge_dir = self.resource_dir.join("assets/tmwd_cdp_bridge");
        if !src_bridge_dir.exists() {
            return Ok(());
        }
        let bridge_dir = self.browser_dir.join("tmwd_cdp_bridge");
        copy_dir_missing(&src_bridge_dir, &bridge_dir)?;
        let config = bridge_dir.join("config.js");
        if config.exists() {
            return Ok(());
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let tid = format!(
            "__ljq_{:06x}",
            (nanos ^ std::process::id() as u128) & 0x00ff_ffff
        );
        fs::write(config, format!("const TID = '{tid}';\n"))?;
        Ok(())
    }

    pub fn redacted(&self) -> Self {
        let mut c = self.clone();
        c.openai_api_key = redact_secret(&c.openai_api_key);
        for llm in &mut c.llm_configs {
            llm.api_key = redact_secret(&llm.api_key);
        }
        c.custom_headers = redact_headers(&c.custom_headers);
        for llm in &mut c.llm_configs {
            llm.custom_headers = redact_headers(&llm.custom_headers);
        }
        c
    }
}

fn load_llm_model_configs(
    root_dir: &Path,
    primary: &LlmModelConfig,
    default_entry: Option<&LlmEntry>,
    legacy_entries: &[LlmEntry],
) -> Vec<LlmModelConfig> {
    let parsed = fs::read_to_string(root_dir.join("config/llms.toml"))
        .ok()
        .and_then(|s| toml::from_str::<LlmToml>(&s).ok());
    let mut out = vec![primary.clone()];
    let mut entries = parsed
        .as_ref()
        .and_then(|c| c.models.as_ref().or(c.llms.as_ref()))
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        entries = legacy_entries.to_vec();
    }
    for (idx, entry) in entries.into_iter().enumerate() {
        let model = entry.model.clone().unwrap_or_else(|| primary.model.clone());
        let cfg = LlmModelConfig {
            name: entry
                .name
                .clone()
                .unwrap_or_else(|| format!("{idx}:{model}")),
            base_url: entry
                .base_url
                .clone()
                .or_else(|| default_entry.and_then(|d| d.base_url.clone()))
                .unwrap_or_else(|| primary.base_url.clone()),
            api_key: entry
                .api_key
                .clone()
                .or_else(|| default_entry.and_then(|d| d.api_key.clone()))
                .unwrap_or_else(|| primary.api_key.clone()),
            model,
            api_style: entry
                .api_style
                .clone()
                .or_else(|| default_entry.and_then(|d| d.api_style.clone()))
                .unwrap_or_else(|| primary.api_style.clone()),
            auth_scheme: entry_auth_scheme(&entry)
                .or_else(|| default_entry.and_then(entry_auth_scheme))
                .or_else(|| primary.auth_scheme.clone()),
            auth_header: entry_auth_header(&entry)
                .or_else(|| default_entry.and_then(entry_auth_header))
                .or_else(|| primary.auth_header.clone()),
            stream: entry
                .stream
                .or_else(|| default_entry.and_then(|d| d.stream))
                .unwrap_or(primary.stream),
            timeout_secs: entry
                .timeout_secs
                .or_else(|| default_entry.and_then(|d| d.timeout_secs))
                .unwrap_or(primary.timeout_secs),
            connect_timeout_secs: entry
                .connect_timeout_secs
                .or(entry.connect_timeout)
                .or_else(|| default_entry.and_then(entry_connect_timeout))
                .unwrap_or(primary.connect_timeout_secs)
                .max(1),
            verify_tls: entry
                .verify_tls
                .or(entry.verify)
                .or_else(|| default_entry.and_then(entry_verify_tls))
                .unwrap_or(primary.verify_tls),
            temperature: entry
                .temperature
                .or_else(|| default_entry.and_then(|d| d.temperature))
                .or(primary.temperature),
            max_tokens: entry
                .max_tokens
                .or_else(|| default_entry.and_then(|d| d.max_tokens))
                .or(primary.max_tokens),
            reasoning_effort: entry
                .reasoning_effort
                .clone()
                .or_else(|| default_entry.and_then(|d| d.reasoning_effort.clone()))
                .or_else(|| primary.reasoning_effort.clone())
                .and_then(valid_reasoning_effort),
            thinking_type: entry
                .thinking_type
                .clone()
                .or_else(|| default_entry.and_then(|d| d.thinking_type.clone()))
                .or_else(|| primary.thinking_type.clone())
                .and_then(valid_thinking_type),
            thinking_budget_tokens: entry
                .thinking_budget_tokens
                .or_else(|| default_entry.and_then(|d| d.thinking_budget_tokens))
                .or(primary.thinking_budget_tokens),
            service_tier: entry
                .service_tier
                .clone()
                .or_else(|| default_entry.and_then(|d| d.service_tier.clone()))
                .or_else(|| primary.service_tier.clone())
                .and_then(valid_service_tier),
            proxy: entry
                .proxy
                .clone()
                .or_else(|| default_entry.and_then(|d| d.proxy.clone()))
                .or_else(|| primary.proxy.clone()),
            failover: entry
                .failover
                .or_else(|| default_entry.and_then(|d| d.failover))
                .unwrap_or(primary.failover),
            custom_headers: merge_headers(
                &primary.custom_headers,
                default_entry.and_then(entry_headers).as_ref(),
                entry_headers(&entry).as_ref(),
            ),
        };
        let duplicate_primary = cfg.base_url == primary.base_url
            && cfg.api_key == primary.api_key
            && cfg.model == primary.model
            && cfg.api_style == primary.api_style;
        if !duplicate_primary {
            out.push(cfg);
        }
    }
    out
}

fn profile_to_model_config(
    profile: &LlmEntry,
    model_alias: &str,
    defaults: Option<&LlmEntry>,
) -> Result<LlmModelConfig> {
    let profile_name = profile.name.clone().context("profile name missing")?;
    if profile.model.is_some() {
        bail!("profile `{profile_name}` uses old `model` field; rewrite it as [[profiles.models]]");
    }
    let models = profile
        .models
        .as_ref()
        .with_context(|| format!("profile `{profile_name}` has no [[profiles.models]] entries"))?;
    let model_entry = models
        .iter()
        .find(|m| m.name.as_deref() == Some(model_alias))
        .with_context(|| {
            format!("profile `{profile_name}` model `{model_alias}` does not exist")
        })?;
    let model = model_entry
        .id
        .clone()
        .or_else(|| model_entry.model.clone())
        .with_context(|| format!("profile `{profile_name}` model `{model_alias}` id missing"))?;
    let name = format!("{profile_name}:{model_alias}");
    let base_url = profile
        .base_url
        .clone()
        .or_else(|| defaults.and_then(|d| d.base_url.clone()))
        .with_context(|| format!("profile `{name}` base_url missing"))?;
    let api_key_env = profile
        .api_key_env
        .clone()
        .or_else(|| defaults.and_then(|d| d.api_key_env.clone()));
    let api_key = profile
        .api_key
        .clone()
        .or_else(|| defaults.and_then(|d| d.api_key.clone()))
        .or_else(|| api_key_env.as_ref().and_then(|key| env::var(key).ok()))
        .with_context(|| {
            api_key_env
                .as_ref()
                .map(|key| {
                    format!(
                        "profile `{name}` missing API key; run `koda-agent config secret {key}`"
                    )
                })
                .unwrap_or_else(|| format!("profile `{name}` api_key_env missing"))
        })?;
    let api_style = profile_api_style(profile, defaults);
    let custom_headers = merge_headers(
        &BTreeMap::new(),
        defaults.and_then(entry_headers).as_ref(),
        entry_headers(profile).as_ref(),
    );
    Ok(LlmModelConfig {
        name,
        base_url,
        api_key,
        model,
        api_style,
        auth_scheme: entry_auth_scheme(profile).or_else(|| defaults.and_then(entry_auth_scheme)),
        auth_header: entry_auth_header(profile).or_else(|| defaults.and_then(entry_auth_header)),
        stream: model_entry
            .stream
            .or(profile.stream)
            .or_else(|| defaults.and_then(|d| d.stream))
            .unwrap_or(true),
        timeout_secs: model_entry
            .timeout_secs
            .or(profile.timeout_secs)
            .or_else(|| defaults.and_then(|d| d.timeout_secs))
            .unwrap_or(600),
        connect_timeout_secs: entry_model_connect_timeout(model_entry)
            .or_else(|| entry_connect_timeout(profile))
            .or_else(|| defaults.and_then(entry_connect_timeout))
            .unwrap_or(30)
            .max(1),
        verify_tls: entry_model_verify_tls(model_entry)
            .or_else(|| entry_verify_tls(profile))
            .or_else(|| defaults.and_then(entry_verify_tls))
            .unwrap_or(true),
        temperature: model_entry
            .temperature
            .or(profile.temperature)
            .or_else(|| defaults.and_then(|d| d.temperature)),
        max_tokens: model_entry
            .max_tokens
            .or(profile.max_tokens)
            .or_else(|| defaults.and_then(|d| d.max_tokens)),
        reasoning_effort: model_entry
            .reasoning_effort
            .clone()
            .or_else(|| profile.reasoning_effort.clone())
            .or_else(|| defaults.and_then(|d| d.reasoning_effort.clone()))
            .and_then(valid_reasoning_effort),
        thinking_type: model_entry
            .thinking_type
            .clone()
            .or_else(|| profile.thinking_type.clone())
            .or_else(|| defaults.and_then(|d| d.thinking_type.clone()))
            .and_then(valid_thinking_type),
        thinking_budget_tokens: model_entry
            .thinking_budget_tokens
            .or(profile.thinking_budget_tokens)
            .or_else(|| defaults.and_then(|d| d.thinking_budget_tokens)),
        service_tier: model_entry
            .service_tier
            .clone()
            .or_else(|| profile.service_tier.clone())
            .or_else(|| defaults.and_then(|d| d.service_tier.clone()))
            .and_then(valid_service_tier),
        proxy: model_entry
            .proxy
            .clone()
            .or_else(|| profile.proxy.clone())
            .or_else(|| defaults.and_then(|d| d.proxy.clone())),
        failover: model_entry
            .failover
            .or(profile.failover)
            .or_else(|| defaults.and_then(|d| d.failover))
            .unwrap_or(true),
        custom_headers: merge_headers(
            &custom_headers,
            entry_model_headers(model_entry).as_ref(),
            None,
        ),
    })
}

fn profile_api_style(profile: &LlmEntry, defaults: Option<&LlmEntry>) -> String {
    if let Some(style) = profile
        .api_style
        .clone()
        .or_else(|| defaults.and_then(|d| d.api_style.clone()))
    {
        return style;
    }
    let kind = profile
        .kind
        .as_deref()
        .or_else(|| defaults.and_then(|d| d.kind.as_deref()))
        .unwrap_or("native_oai");
    if matches!(kind, "native_claude" | "claude") {
        return "claude".into();
    }
    match profile
        .api_mode
        .as_deref()
        .or_else(|| defaults.and_then(|d| d.api_mode.as_deref()))
    {
        Some("responses" | "response") => "responses".into(),
        _ => "chat".into(),
    }
}

fn entry_model_connect_timeout(entry: &LlmModelEntry) -> Option<u64> {
    entry.connect_timeout_secs.or(entry.connect_timeout)
}

fn entry_model_verify_tls(entry: &LlmModelEntry) -> Option<bool> {
    entry.verify_tls.or(entry.verify)
}

fn entry_model_headers(entry: &LlmModelEntry) -> Option<BTreeMap<String, String>> {
    entry
        .headers
        .clone()
        .or_else(|| entry.custom_headers.clone())
}

fn entry_auth_scheme(entry: &LlmEntry) -> Option<String> {
    entry
        .auth_scheme
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_ascii_lowercase())
}

fn entry_auth_header(entry: &LlmEntry) -> Option<String> {
    entry
        .auth_header
        .as_deref()
        .or(entry.api_key_header.as_deref())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
}

fn load_mixin_config(root_dir: &Path) -> Option<MixinConfig> {
    let parsed = fs::read_to_string(root_dir.join("config/llms.toml"))
        .ok()
        .and_then(|s| toml::from_str::<LlmToml>(&s).ok())
        .and_then(|c| c.mixin);
    let mut cfg = parsed.as_ref().map(|_| MixinConfig::default());
    if let Some(parsed) = parsed
        && let Some(cfg) = &mut cfg
    {
        if let Some(llm_nos) = parsed.llm_nos {
            cfg.llm_nos = llm_nos
                .into_iter()
                .filter_map(|v| match v {
                    toml::Value::Integer(i) => Some(i.to_string()),
                    toml::Value::String(s) if !s.trim().is_empty() => Some(s),
                    _ => None,
                })
                .collect();
        }
        if let Some(max_retries) = parsed.max_retries {
            cfg.max_retries = max_retries;
        }
        if let Some(base_delay) = parsed.base_delay.or(parsed.base_delay_secs) {
            cfg.base_delay_secs = base_delay.max(0.0);
        }
        if let Some(spring_back) = parsed.spring_back.or(parsed.spring_back_secs) {
            cfg.spring_back_secs = spring_back;
        }
    }
    if let Ok(raw) = env::var("KODA_MIXIN_LLMS") {
        let cfg = cfg.get_or_insert_with(MixinConfig::default);
        cfg.llm_nos = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
    if let Ok(raw) = env::var("KODA_MIXIN_MAX_RETRIES")
        && let Ok(v) = raw.parse()
    {
        let cfg = cfg.get_or_insert_with(MixinConfig::default);
        cfg.max_retries = v;
    }
    if let Ok(raw) = env::var("KODA_MIXIN_BASE_DELAY_SECS")
        && let Ok(v) = raw.parse::<f64>()
    {
        let cfg = cfg.get_or_insert_with(MixinConfig::default);
        cfg.base_delay_secs = v.max(0.0);
    }
    if let Ok(raw) = env::var("KODA_MIXIN_SPRING_BACK_SECS")
        && let Ok(v) = raw.parse()
    {
        let cfg = cfg.get_or_insert_with(MixinConfig::default);
        cfg.spring_back_secs = v;
    }
    cfg
}

fn load_legacy_mykey_config(root_dir: &Path) -> LegacyMykeyConfig {
    if fs::metadata(root_dir.join("config/llms.toml")).is_ok() {
        return LegacyMykeyConfig::default();
    }
    let values = load_mykey_json(root_dir)
        .or_else(|| load_mykey_py(root_dir))
        .unwrap_or_default();
    legacy_values_to_config(values)
}

fn load_mykey_json(root_dir: &Path) -> Option<BTreeMap<String, Value>> {
    let path = root_dir.join("mykey.json");
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn load_mykey_py(root_dir: &Path) -> Option<BTreeMap<String, Value>> {
    let text = fs::read_to_string(root_dir.join("mykey.py")).ok()?;
    let mut out = BTreeMap::new();
    for (name, raw) in extract_python_dict_assignments(&text) {
        if let Some(jsonish) = python_literal_to_jsonish(&raw)
            && let Ok(value) = serde_json::from_str::<Value>(&jsonish)
        {
            out.insert(name, value);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn legacy_values_to_config(values: BTreeMap<String, Value>) -> LegacyMykeyConfig {
    let mut cfg = LegacyMykeyConfig::default();
    for (name, value) in values {
        let lname = name.to_ascii_lowercase();
        let Some(obj) = value.as_object() else {
            continue;
        };
        if lname.contains("mixin") {
            cfg.mixin = Some(MixinConfig {
                llm_nos: obj
                    .get("llm_nos")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| {
                                v.as_str()
                                    .map(str::to_string)
                                    .or_else(|| v.as_i64().map(|n| n.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                max_retries: obj.get("max_retries").and_then(Value::as_u64).unwrap_or(3) as usize,
                base_delay_secs: obj
                    .get("base_delay")
                    .or_else(|| obj.get("base_delay_secs"))
                    .and_then(Value::as_f64)
                    .unwrap_or(1.5),
                spring_back_secs: obj
                    .get("spring_back")
                    .or_else(|| obj.get("spring_back_secs"))
                    .and_then(Value::as_u64)
                    .unwrap_or(300),
            });
            continue;
        }
        if !["api", "config", "cookie"]
            .iter()
            .any(|needle| lname.contains(needle))
        {
            continue;
        }
        if let Some(entry) = legacy_obj_to_entry(&lname, obj) {
            if cfg.default.is_none() {
                cfg.default = Some(entry.clone());
            }
            cfg.models.push(entry);
        }
    }
    if let Some(first) = cfg.mixin.as_ref().and_then(|m| m.llm_nos.first())
        && let Some(entry) = cfg
            .models
            .iter()
            .find(|entry| entry.name.as_deref() == Some(first.as_str()))
    {
        cfg.default = Some(entry.clone());
    }
    cfg
}

fn legacy_obj_to_entry(key_name: &str, obj: &serde_json::Map<String, Value>) -> Option<LlmEntry> {
    let api_key = string_field(obj, &["api_key", "apikey", "key"])?;
    let base_url = string_field(obj, &["base_url", "apibase", "api_base"])?;
    let model = string_field(obj, &["model"])?;
    let api_style = if key_name.contains("native_claude") || key_name.contains("claude") {
        "claude".to_string()
    } else if obj
        .get("api_mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode.eq_ignore_ascii_case("responses"))
    {
        "responses".to_string()
    } else if key_name.contains("native_oai") {
        "chat".to_string()
    } else {
        "text".to_string()
    };
    let custom_headers = legacy_custom_headers(key_name, obj);
    Some(LlmEntry {
        name: string_field(obj, &["name"]).or_else(|| Some(model.clone())),
        kind: None,
        base_url: Some(base_url),
        api_key: Some(api_key),
        api_key_env: None,
        auth_scheme: None,
        auth_header: None,
        api_key_header: None,
        model: Some(model),
        models: None,
        api_style: Some(api_style),
        api_mode: string_field(obj, &["api_mode"]),
        max_turns: obj
            .get("max_turns")
            .or_else(|| obj.get("max_turn"))
            .and_then(Value::as_u64)
            .map(|v| v as usize),
        stream: obj.get("stream").and_then(Value::as_bool),
        timeout_secs: obj
            .get("read_timeout")
            .or_else(|| obj.get("timeout_secs"))
            .or_else(|| obj.get("timeout"))
            .and_then(Value::as_u64),
        connect_timeout_secs: obj
            .get("connect_timeout")
            .or_else(|| obj.get("connect_timeout_secs"))
            .and_then(Value::as_u64),
        connect_timeout: None,
        verify_tls: obj
            .get("verify_tls")
            .or_else(|| obj.get("verify"))
            .and_then(Value::as_bool),
        verify: None,
        temperature: obj.get("temperature").and_then(Value::as_f64),
        max_tokens: obj.get("max_tokens").and_then(Value::as_u64),
        reasoning_effort: string_field(obj, &["reasoning_effort"]),
        thinking_type: string_field(obj, &["thinking_type"]),
        thinking_budget_tokens: obj.get("thinking_budget_tokens").and_then(Value::as_u64),
        service_tier: string_field(obj, &["service_tier"]),
        proxy: string_field(obj, &["proxy"]),
        failover: obj.get("failover").and_then(Value::as_bool),
        headers: custom_headers.clone(),
        custom_headers,
    })
}

fn legacy_custom_headers(
    key_name: &str,
    obj: &serde_json::Map<String, Value>,
) -> Option<BTreeMap<String, String>> {
    let mut headers = BTreeMap::new();
    if let Some(map) = obj
        .get("headers")
        .or_else(|| obj.get("custom_headers"))
        .and_then(Value::as_object)
    {
        for (k, v) in map {
            if let Some(v) = v.as_str() {
                headers.insert(k.clone(), v.to_string());
            }
        }
    }
    if let Some(user_agent) = string_field(obj, &["user_agent"]) {
        headers.insert("user-agent".into(), user_agent);
    }
    if key_name.contains("native_claude") {
        headers
            .entry("anthropic-dangerous-direct-browser-access".into())
            .or_insert_with(|| "true".into());
        headers
            .entry("x-app".into())
            .or_insert_with(|| "cli".into());
    }
    (!headers.is_empty()).then_some(headers)
}

fn string_field(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_str).map(str::to_string))
        .filter(|s| !s.trim().is_empty())
}

fn extract_python_dict_assignments(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut idx = 0;
    while let Some(eq_rel) = text[idx..].find('=') {
        let eq = idx + eq_rel;
        let prefix = text[..eq].trim_end();
        let name_start = prefix
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map(|i| i + 1)
            .unwrap_or(0);
        let name = prefix[name_start..].trim();
        if name.is_empty() || name.starts_with('#') {
            idx = eq + 1;
            continue;
        }
        let Some(open_rel) = text[eq + 1..].find('{') else {
            break;
        };
        let open = eq + 1 + open_rel;
        let mut depth = 0i32;
        let mut quote = None;
        let mut escaped = false;
        let mut end = None;
        for (pos, byte) in bytes.iter().enumerate().skip(open) {
            let ch = *byte as char;
            if let Some(q) = quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == q {
                    quote = None;
                }
                continue;
            }
            match ch {
                '\'' | '"' => quote = Some(ch),
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(pos + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(end) = end {
            out.push((name.to_string(), text[open..end].to_string()));
            idx = end;
        } else {
            break;
        }
    }
    out
}

fn python_literal_to_jsonish(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' | '"' => {
                let quote = ch;
                let mut value = String::new();
                let mut escaped = false;
                for c in chars.by_ref() {
                    if escaped {
                        value.push(c);
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == quote {
                        break;
                    } else {
                        value.push(c);
                    }
                }
                out.push_str(&serde_json::to_string(&value).ok()?);
            }
            '#' => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            _ => out.push(ch),
        }
    }
    let jsonish = out
        .replace("True", "true")
        .replace("False", "false")
        .replace("None", "null");
    Some(remove_trailing_json_commas(&jsonish))
}

fn remove_trailing_json_commas(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut quote = None;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' => {
                quote = Some(ch);
                out.push(ch);
            }
            ',' => {
                let mut lookahead = chars.clone();
                while matches!(lookahead.peek(), Some(c) if c.is_whitespace()) {
                    lookahead.next();
                }
                if !matches!(lookahead.peek(), Some('}' | ']')) {
                    out.push(ch);
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

fn entry_headers(entry: &LlmEntry) -> Option<BTreeMap<String, String>> {
    entry
        .custom_headers
        .clone()
        .or_else(|| entry.headers.clone())
        .filter(|h| !h.is_empty())
}

fn entry_connect_timeout(entry: &LlmEntry) -> Option<u64> {
    entry.connect_timeout_secs.or(entry.connect_timeout)
}

fn entry_verify_tls(entry: &LlmEntry) -> Option<bool> {
    entry.verify_tls.or(entry.verify)
}

fn merge_headers(
    primary: &BTreeMap<String, String>,
    default: Option<&BTreeMap<String, String>>,
    entry: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    let mut out = primary.clone();
    if let Some(default) = default {
        out.extend(default.clone());
    }
    if let Some(entry) = entry {
        out.extend(entry.clone());
    }
    out
}

fn valid_reasoning_effort(raw: String) -> Option<String> {
    let v = raw.trim().to_ascii_lowercase();
    matches!(
        v.as_str(),
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
    )
    .then_some(v)
}

fn valid_service_tier(raw: String) -> Option<String> {
    let v = raw.trim().to_ascii_lowercase();
    matches!(v.as_str(), "auto" | "default" | "priority" | "flex").then_some(v)
}

fn valid_thinking_type(raw: String) -> Option<String> {
    let v = raw.trim().to_ascii_lowercase();
    matches!(v.as_str(), "adaptive" | "enabled" | "disabled").then_some(v)
}

fn redact_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| {
            let sensitive = {
                let kl = k.to_ascii_lowercase();
                kl.contains("authorization")
                    || kl.contains("api-key")
                    || kl.contains("apikey")
                    || kl.contains("token")
                    || kl.contains("secret")
            };
            (
                k.clone(),
                if sensitive {
                    redact_secret(v)
                } else {
                    v.clone()
                },
            )
        })
        .collect()
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub fn redact_secret(s: &str) -> String {
    let chars = s.chars().collect::<Vec<_>>();
    if chars.len() <= 8 {
        "<redacted>".into()
    } else {
        let head = chars.iter().take(4).collect::<String>();
        let tail = chars
            .iter()
            .skip(chars.len().saturating_sub(4))
            .collect::<String>();
        format!("{head}...{tail}")
    }
}

fn has_api_version(url: &str) -> bool {
    // Check if the last path segment is a version like /v1, /v2, /v4, etc.
    url.rsplit('/').next().is_some_and(|seg| {
        seg.starts_with('v') && seg.len() > 1 && seg[1..].bytes().all(|b| b.is_ascii_digit())
    })
}

pub fn auto_make_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with(path) {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/{path}")
    } else if (path == "chat/completions" || path == "messages" || path == "responses")
        && !has_api_version(base)
    {
        format!("{base}/v1/{path}")
    } else {
        format!("{base}/{path}")
    }
}

pub fn smart_format(data: &str, max_str_len: usize, omit: &str) -> String {
    let total = data.chars().count();
    if total < max_str_len + omit.chars().count() * 2 {
        return data.to_string();
    }
    let half = max_str_len / 2;
    let head = data.chars().take(half).collect::<String>();
    let tail = data
        .chars()
        .skip(total.saturating_sub(half))
        .collect::<String>();
    format!("{head}{omit}{tail}")
}

pub fn load_tool_schema(root: &Path, suffix: &str) -> Result<Value> {
    let path = root.join(format!("assets/tools_schema{suffix}.json"));
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

pub fn system_prompt(root: &Path) -> Result<String> {
    let mut prompt = fs::read_to_string(root.join("assets/sys_prompt.txt"))
        .unwrap_or_else(|_| "You are GenericAgent.".into());
    prompt.push_str(&format!(
        "\nToday: {}\n",
        Local::now().format("%Y-%m-%d %a")
    ));
    Ok(prompt)
}

fn consume_file(dir: &Path, name: &str) -> Option<String> {
    let path = dir.join(name);
    let content = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(path);
    let trimmed = content.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn global_memory_prompt(cfg: &AgentConfig) -> String {
    let suffix = if env::var("GA_LANG").unwrap_or_default() == "en" {
        "_en"
    } else {
        ""
    };
    let insight = match fs::read_to_string(cfg.memory_dir.join("global_mem_insight.txt")) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let structure = match fs::read_to_string(
        cfg.resource_dir
            .join(format!("assets/insight_fixed_structure{suffix}.txt")),
    ) {
        Ok(v) => v,
        Err(_) => return String::new(),
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

fn resource_memory_prompt(cfg: &AgentConfig) -> String {
    let dir = cfg.resource_dir.join("memory");
    if !dir.is_dir() {
        return String::new();
    }
    let mut files = Vec::new();
    collect_resource_memory_files(&dir, &dir, &mut files);
    files.sort();
    files.truncate(80);
    format!(
        "\n[Resource Memory] 静态 SOP/helper 资源目录: {}。这不是用户长期记忆；需要 SOP/helper 时优先读取这些文件。可用资源示例: {}\n",
        dir.display(),
        files.join(", ")
    )
}

fn collect_resource_memory_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_resource_memory_files(root, &path, out);
        } else if path.is_file()
            && let Some(ext) = path.extension().and_then(|s| s.to_str())
            && matches!(ext, "md" | "py" | "txt")
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.display().to_string());
        }
    }
}

fn with_runtime_recall_prompt(cfg: &AgentConfig, user_input: &str) -> String {
    if !should_recall_l4(user_input) {
        return user_input.to_string();
    }
    let hits = recall_l4_history_core(cfg, user_input, 2);
    if hits.is_empty() {
        return user_input.to_string();
    }
    let rendered = hits
        .into_iter()
        .map(|hit| {
            format!(
                "- session: {}; score: {}; excerpt: {}",
                hit.session, hit.score, hit.excerpt
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "<system-recall>\n以下是按当前问题关键词检索到的历史会话片段，只作为线索；不要当作已验证事实，必要时重新读取文件/运行工具核实。\n{rendered}\n</system-recall>\n\n{user_input}"
    )
}

fn should_recall_l4(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    [
        "之前", "上次", "历史", "记忆", "继续", "刚才", "前面", "previous", "before", "history",
        "recall", "remember", "continue", "resume",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CoreL4RecallHit {
    session: String,
    score: usize,
    excerpt: String,
}

fn recall_l4_history_core(cfg: &AgentConfig, query: &str, limit: usize) -> Vec<CoreL4RecallHit> {
    let terms = recall_terms(query);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut hits = l4_recall_blocks_core(cfg)
        .into_iter()
        .filter_map(|(session, body)| {
            let lower = body.to_ascii_lowercase();
            let score = terms
                .iter()
                .map(|term| lower.matches(term).count())
                .sum::<usize>();
            (score > 0).then(|| CoreL4RecallHit {
                session,
                score,
                excerpt: clean_recall_excerpt(&excerpt_for_recall(&body, &terms, 360)),
            })
        })
        .collect::<Vec<_>>();
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.session.cmp(&a.session))
    });
    hits.truncate(limit.max(1));
    hits
}

fn recall_terms(query: &str) -> Vec<String> {
    let mut terms = query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && !is_cjk_char(c))
        .map(str::trim)
        .filter(|s| s.chars().count() >= 2)
        .map(str::to_ascii_lowercase)
        .filter(|s| {
            !matches!(
                s.as_str(),
                "之前"
                    | "上次"
                    | "历史"
                    | "记忆"
                    | "继续"
                    | "刚才"
                    | "前面"
                    | "previous"
                    | "before"
                    | "history"
                    | "recall"
                    | "remember"
                    | "continue"
                    | "resume"
            )
        })
        .collect::<Vec<_>>();
    terms.sort();
    terms.dedup();
    terms
}

fn l4_recall_blocks_core(cfg: &AgentConfig) -> Vec<(String, String)> {
    let dir = cfg.memory_dir.join("L4_raw_sessions");
    let mut out = split_l4_history_blocks_core(
        &fs::read_to_string(dir.join("all_histories.txt")).unwrap_or_default(),
    );
    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let session = value
                .get("session")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let mut body = Vec::new();
            if let Some(history) = value.get("history").and_then(Value::as_array) {
                body.extend(history.iter().filter_map(Value::as_str).map(str::to_string));
            }
            if !session.is_empty() && !body.is_empty() {
                out.push((session, body.join("\n")));
            }
        }
    }
    out
}

fn split_l4_history_blocks_core(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let markers = text
        .match_indices("SESSION: ")
        .map(|(idx, _)| idx)
        .collect::<Vec<_>>();
    for (i, start) in markers.iter().copied().enumerate() {
        let line_end = text[start..]
            .find('\n')
            .map(|n| start + n)
            .unwrap_or(text.len());
        let session = text[start + "SESSION: ".len()..line_end].trim().to_string();
        let body_end = markers.get(i + 1).copied().unwrap_or(text.len());
        let body = text[line_end..body_end].trim().to_string();
        if !session.is_empty() && !body.is_empty() {
            out.push((session, body));
        }
    }
    out
}

fn excerpt_for_recall(text: &str, terms: &[String], max_chars: usize) -> String {
    let lower = text.to_ascii_lowercase();
    let idx = terms
        .iter()
        .filter_map(|term| lower.find(term))
        .min()
        .unwrap_or(0);
    let start = text[..idx]
        .char_indices()
        .rev()
        .nth(60)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let excerpt = text[start..].chars().take(max_chars).collect::<String>();
    if start > 0 {
        format!("...{}", excerpt.trim())
    } else {
        excerpt.trim().to_string()
    }
}

fn clean_recall_excerpt(text: &str) -> String {
    text.replace(['\n', '\r', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(360)
        .collect()
}

fn is_cjk_char(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
}

fn append_model_log(cfg: &AgentConfig, label: &str, body: &str) -> Result<()> {
    let dir = cfg.temp_dir.join("model_responses");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("model_responses_{}.txt", std::process::id()));
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        f,
        "=== {label} === {}",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    )?;
    writeln!(f, "{body}")?;
    if langfuse_trace_enabled(cfg) {
        append_langfuse_trace(
            cfg,
            "llm.chat",
            "generation",
            match label {
                "Prompt" => "start",
                "Response" => "end",
                other => other,
            },
            json!({"label":label,"content":smart_format(body, 20_000, "\n...[truncated langfuse trace]...\n")}),
        )?;
    }
    Ok(())
}

fn langfuse_trace_enabled(cfg: &AgentConfig) -> bool {
    env::var("KODA_LANGFUSE_TRACE")
        .ok()
        .and_then(|v| parse_bool(&v))
        .unwrap_or(false)
        || cfg.home_dir.join("config/langfuse.toml").exists()
        || cfg.home_dir.join("langfuse_config.json").exists()
}

fn append_langfuse_trace(
    cfg: &AgentConfig,
    name: &str,
    observation_type: &str,
    phase: &str,
    payload: Value,
) -> Result<()> {
    fs::create_dir_all(&cfg.logs_dir)?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.logs_dir.join("langfuse_trace.jsonl"))?;
    writeln!(
        f,
        "{}",
        serde_json::to_string(&json!({
            "ts": Local::now().to_rfc3339(),
            "name": name,
            "type": observation_type,
            "phase": phase,
            "payload": payload,
        }))?
    )?;
    Ok(())
}

fn append_history_log(cfg: &AgentConfig, history: &[String]) -> Result<()> {
    if history.is_empty() {
        return Ok(());
    }
    let body = format!("<history>\n{}\n</history>", history.join("\n"));
    append_model_log(cfg, "History", &body)
}

fn archive_l4_session(
    cfg: &AgentConfig,
    history: &[String],
    messages: &[ChatMessage],
) -> Result<()> {
    if history.len() < 2 {
        return Ok(());
    }
    let dir = cfg.memory_dir.join("L4_raw_sessions");
    fs::create_dir_all(&dir)?;
    let ts = Local::now().format("%Y%m%d_%H%M%S");
    let session_name = format!("session_{}_{}", ts, std::process::id());
    let payload = json!({
        "session": session_name,
        "created_at": Local::now().to_rfc3339(),
        "history": history,
        "messages": messages,
    });
    fs::write(
        dir.join(format!("{session_name}.json")),
        serde_json::to_vec_pretty(&payload)?,
    )?;
    append_all_histories(&dir, &session_name, history)?;
    Ok(())
}

fn append_all_histories(dir: &Path, session_name: &str, history: &[String]) -> Result<()> {
    let path = dir.join("all_histories.txt");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.contains(&format!("SESSION: {session_name}\n")) {
        return Ok(());
    }
    let sep = "=".repeat(60);
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{sep}")?;
    writeln!(f, "SESSION: {session_name}")?;
    writeln!(f, "{sep}")?;
    for line in history {
        writeln!(f, "{line}")?;
    }
    writeln!(f)?;
    Ok(())
}

fn snapshot_prompt(messages: &[ChatMessage]) -> String {
    serde_json::to_string(messages).unwrap_or_else(|_| "[]".into())
}

fn strip_tag_blocks(text: &str, tag: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    while let Some(i) = rest.find(&open) {
        out.push_str(&rest[..i]);
        let after = &rest[i + open.len()..];
        if let Some(j) = after.find(&close) {
            rest = &after[j + close.len()..];
        } else {
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn extract_summary(text: &str) -> Option<String> {
    let start = text.find("<summary>")? + "<summary>".len();
    let end = text[start..].find("</summary>")? + start;
    Some(text[start..end].trim().replace('\n', ""))
}

fn content_without_summary(text: &str) -> String {
    strip_tag_blocks(text, "summary").trim().to_string()
}

fn display_content_with_summary(text: &str) -> String {
    let body = content_without_summary(text);
    match extract_summary(text) {
        Some(summary) if !summary.trim().is_empty() && !body.is_empty() => {
            format!("💭 {summary}\n\n{body}")
        }
        Some(summary) if !summary.trim().is_empty() => format!("💭 {summary}"),
        _ => body,
    }
}

fn emit_response_annotations(
    turn: usize,
    response: &AgentResponse,
    saw_thinking_delta: bool,
    emit: &(impl Fn(AgentEvent) + Send + Sync),
) {
    if !saw_thinking_delta && !response.thinking.trim().is_empty() {
        emit(AgentEvent::ThinkingMessage {
            turn,
            content: response.thinking.clone(),
        });
    }
    if let Some(summary) = extract_summary(&response.content)
        && !summary.trim().is_empty()
    {
        emit(AgentEvent::ThinkingMessage {
            turn,
            content: format!("summary: {summary}"),
        });
    }
}

fn no_tool_next_prompt(response: &AgentResponse) -> Option<String> {
    let content = response.content.trim();
    if content.is_empty() && response.thinking.trim().is_empty() {
        return Some("[System] Blank response, regenerate and tooluse".into());
    }
    let tail = content
        .chars()
        .rev()
        .take(100)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    if tail.contains("[!!! 流异常中断") || tail.contains("!!!Error:") {
        return Some("[System] Incomplete response. Regenerate and tooluse.".into());
    }
    if tail.contains("max_tokens !!!]") {
        return Some("[System] max_tokens limit reached. Use multi small steps to do it.".into());
    }
    if likely_large_code_only(content) {
        return Some(
            "[System] 检测到你在上一轮回复中主要内容是较大代码块，且本轮未调用任何工具。\n\
             如果这些代码需要执行、写入文件或进一步分析，请重新组织回复并显式调用相应工具\
             （例如：code_run、file_write、file_patch 等）；\n\
             如果只是向用户展示或讲解代码片段，请在回复中补充自然语言说明，\
             并明确是否还需要额外的实际操作。"
                .into(),
        );
    }
    None
}

fn likely_large_code_only(content: &str) -> bool {
    let Some(first) = content.find("```") else {
        return false;
    };
    let after_first = &content[first + 3..];
    let Some(close_rel) = after_first.find("```") else {
        return false;
    };
    let close = first + 3 + close_rel + 3;
    if !content[close..].trim().is_empty() {
        return false;
    }
    if content[close..].contains("```") {
        return false;
    }
    let code = &content[first..close];
    if code.chars().count() < 50 {
        return false;
    }
    let residual = strip_tag_blocks(&content[..first], "thinking");
    let residual = strip_tag_blocks(&residual, "summary");
    residual
        .split_whitespace()
        .collect::<String>()
        .chars()
        .count()
        <= 30
}

fn fallback_turn_summary(response: &AgentResponse, tool_calls: &[ToolCall]) -> String {
    if let Some(summary) = extract_summary(&response.content) {
        return smart_format(&summary, 80, " ... ");
    }
    if let Some(tc) = tool_calls.first() {
        return smart_format(
            &format!("调用工具{}, args: {}", tc.name, tc.args),
            80,
            " ... ",
        );
    }
    "直接回答了用户问题".into()
}

#[derive(Debug, Clone)]
struct ModelLogSession {
    path: PathBuf,
    modified: std::time::SystemTime,
    rounds: usize,
    preview: String,
    pairs: Vec<(String, String)>,
}

fn recent_model_logs(cfg: &AgentConfig) -> Result<Vec<ModelLogSession>> {
    let dir = cfg.temp_dir.join("model_responses");
    let mut out = Vec::new();
    let current_name = format!("model_responses_{}.txt", std::process::id());
    let Ok(rd) = fs::read_dir(dir) else {
        return Ok(out);
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if file_name == current_name
            || !file_name.starts_with("model_responses_")
            || !file_name.ends_with(".txt")
            || file_name.starts_with("model_responses_snapshot_")
        {
            continue;
        }
        if !path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("model_responses_") && s.ends_with(".txt"))
        {
            continue;
        }
        let meta = entry.metadata()?;
        let content = fs::read_to_string(&path).unwrap_or_default();
        let pairs = parse_model_log_pairs(&content);
        let rounds = pairs.len().max(content.matches("=== Prompt ===").count());
        let preview = pairs
            .iter()
            .rev()
            .find_map(|(_, r)| last_response_summary(r))
            .or_else(|| pairs.first().map(|(p, _)| preview_prompt(p)))
            .or_else(|| {
                content
                    .lines()
                    .find(|l| !l.starts_with("===") && !l.trim().is_empty())
                    .map(|s| s.chars().take(80).collect())
            })
            .unwrap_or_default();
        out.push(ModelLogSession {
            path,
            modified: meta.modified()?,
            rounds,
            preview,
            pairs,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    Ok(out)
}

fn format_continue_list(cfg: &AgentConfig, limit: usize) -> Result<String> {
    let sessions = recent_model_logs(cfg)?;
    if sessions.is_empty() {
        return Ok("❌ 没有可恢复的历史会话".into());
    }
    let mut lines = vec![
        "**可恢复会话**（输入 `/continue N` 恢复第 N 个）：".to_string(),
        String::new(),
    ];
    for (i, session) in sessions.into_iter().take(limit).enumerate() {
        lines.push(format!(
            "{}. `{}` · **{} 轮** · {}",
            i + 1,
            rel_time(session.modified),
            session.rounds,
            escape_md(
                &session
                    .preview
                    .replace('\n', " ")
                    .chars()
                    .take(60)
                    .collect::<String>()
            )
        ));
    }
    Ok(lines.join("\n"))
}

fn restore_continue_summary(runtime: &AgentRuntime, idx: usize) -> Result<String> {
    let sessions = recent_model_logs(&runtime.cfg)?;
    if idx == 0 || idx > sessions.len() {
        return Ok(format!("❌ 索引越界（有效范围 1-{}）", sessions.len()));
    }
    let session = &sessions[idx - 1];
    let path = &session.path;
    let summary = if session.pairs.is_empty() {
        smart_format(
            &fs::read_to_string(path)?,
            4000,
            "\n\n[omitted restored session]\n\n",
        )
    } else {
        session
            .pairs
            .iter()
            .enumerate()
            .map(|(i, (p, r))| {
                format!(
                    "[ROUND {}]\nUSER: {}\nASSISTANT: {}",
                    i + 1,
                    preview_prompt(p),
                    preview_response(r)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    runtime
        .history_info
        .lock()
        .push(format!("[RESTORED_SUMMARY]: {summary}"));
    Ok(format!(
        "✅ 已恢复 {} 轮会话摘要：{}，请输入新问题继续",
        session.rounds,
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
    ))
}

fn parse_model_log_pairs(content: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut current_label: Option<&str> = None;
    let mut current_body = String::new();
    let mut pending_prompt: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        let label = if trimmed.starts_with("=== Prompt ===") {
            Some("Prompt")
        } else if trimmed.starts_with("=== Response ===") {
            Some("Response")
        } else {
            None
        };
        if let Some(label) = label {
            if let Some(prev) = current_label.take() {
                if prev == "Prompt" {
                    pending_prompt = Some(current_body.trim().to_string());
                } else if prev == "Response"
                    && let Some(prompt) = pending_prompt.take()
                {
                    pairs.push((prompt, current_body.trim().to_string()));
                }
            }
            current_label = Some(label);
            current_body.clear();
        } else {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if let Some("Response") = current_label
        && let Some(prompt) = pending_prompt
    {
        pairs.push((prompt, current_body.trim().to_string()));
    }
    pairs
}

fn preview_prompt(prompt: &str) -> String {
    serde_json::from_str::<Vec<ChatMessage>>(prompt)
        .ok()
        .and_then(|msgs| {
            msgs.into_iter()
                .rev()
                .find(|m| m.role == "user")
                .map(|m| value_text(&m.content))
        })
        .or_else(|| {
            serde_json::from_str::<ChatMessage>(prompt)
                .ok()
                .and_then(|m| (m.role == "user").then(|| value_text(&m.content)))
        })
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| prompt.to_string())
        .replace('\n', " ")
        .chars()
        .take(120)
        .collect()
}

fn last_response_summary(response: &str) -> Option<String> {
    let json_summary = serde_json::from_str::<AgentResponse>(response)
        .ok()
        .and_then(|r| extract_summary(&r.content));
    if json_summary.is_some() {
        return json_summary.map(|s| smart_format(&s, 120, " ... "));
    }
    // Upstream logs often store Python repr blocks:
    // [{'type':'text','text':'...<summary>...</summary>'}]
    extract_summary(response).map(|s| smart_format(&s, 120, " ... "))
}

fn preview_response(response: &str) -> String {
    serde_json::from_str::<AgentResponse>(response)
        .ok()
        .map(|r| {
            if !r.content.trim().is_empty() {
                r.content
            } else if !r.tool_calls.is_empty() {
                format!(
                    "tool calls: {}",
                    r.tool_calls
                        .into_iter()
                        .map(|t| t.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                String::new()
            }
        })
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| response.to_string())
        .replace('\n', " ")
        .chars()
        .take(240)
        .collect()
}

fn escape_md(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if matches!(ch, '\\' | '`' | '*' | '_' | '[' | ']') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn rel_time(modified: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        .as_secs();
    if secs < 60 {
        format!("{secs}秒前")
    } else if secs < 3600 {
        format!("{}分前", secs / 60)
    } else if secs < 86_400 {
        format!("{}小时前", secs / 3600)
    } else {
        format!("{}天前", secs / 86_400)
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Object(map) => map
            .get("content")
            .or_else(|| map.get("text"))
            .map(value_text)
            .unwrap_or_else(|| value.to_string()),
        Value::Array(arr) => arr.iter().map(value_text).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(&self, messages: &[ChatMessage], tools_schema: &Value) -> Result<AgentResponse>;
    async fn chat_with_events(
        &self,
        messages: &[ChatMessage],
        tools_schema: &Value,
        _emit: &(dyn Fn(LlmStreamEvent) + Send + Sync),
        _stop: Option<&AtomicBool>,
    ) -> Result<AgentResponse> {
        self.chat(messages, tools_schema).await
    }
    fn name(&self) -> String;
    fn set_session_option(&self, _key: &str, _value: &Value) -> Result<bool> {
        Ok(false)
    }
    fn list_llms(&self) -> Vec<(usize, String, bool)> {
        vec![(0, self.name(), true)]
    }
    fn switch_llm(&self, n: usize) -> Result<()> {
        if n == 0 {
            Ok(())
        } else {
            bail!("only one LLM configured")
        }
    }
}

#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    async fn dispatch(
        &self,
        name: &str,
        args: Value,
        response: &AgentResponse,
        index: usize,
    ) -> Result<StepOutcome>;
}

#[derive(Clone)]
pub struct AgentRuntime {
    cfg: AgentConfig,
    llm: Arc<dyn LlmClient>,
    tools: Arc<dyn ToolDispatcher>,
    history_info: Arc<Mutex<Vec<String>>>,
    key_info: Arc<Mutex<String>>,
    related_sop: Arc<Mutex<String>>,
    plan_path: Arc<Mutex<Option<PathBuf>>>,
    done_hooks: Arc<Mutex<VecDeque<String>>>,
    messages: Arc<Mutex<Vec<ChatMessage>>>,
    session_overrides: Arc<Mutex<BTreeMap<String, Value>>>,
    stop: Arc<AtomicBool>,
    is_running: Arc<AtomicBool>,
    task_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AgentRuntime {
    pub fn new(
        cfg: AgentConfig,
        llm: Arc<dyn LlmClient>,
        tools: Arc<dyn ToolDispatcher>,
    ) -> Result<Self> {
        cfg.ensure_dirs()?;
        Ok(Self {
            cfg,
            llm,
            tools,
            history_info: Arc::default(),
            key_info: Arc::default(),
            related_sop: Arc::default(),
            plan_path: Arc::default(),
            done_hooks: Arc::default(),
            messages: Arc::default(),
            session_overrides: Arc::default(),
            stop: Arc::new(AtomicBool::new(false)),
            is_running: Arc::new(AtomicBool::new(false)),
            task_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub fn abort(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = fs::create_dir_all(&self.cfg.temp_dir);
        let _ = fs::write(self.cfg.temp_dir.join("_stop_signal"), "1");
    }
    pub fn config(&self) -> &AgentConfig {
        &self.cfg
    }
    pub fn list_llms(&self) -> Vec<(usize, String, bool)> {
        self.llm.list_llms()
    }
    pub fn next_llm(&self, n: usize) -> Result<()> {
        self.llm.switch_llm(n)
    }
    pub fn next_llm_by_name(&self, name: &str) -> Result<usize> {
        let needle = name.trim();
        let (idx, _, _) = self
            .list_llms()
            .into_iter()
            .find(|(_, llm_name, _)| {
                llm_name == needle
                    || llm_name.starts_with(&format!("{needle} ("))
                    || (!needle.contains(':') && llm_name.starts_with(&format!("{needle}:")))
            })
            .with_context(|| format!("LLM profile `{needle}` does not exist"))?;
        self.next_llm(idx)?;
        Ok(idx)
    }
    pub fn next_llm_model_by_name(&self, model_alias: &str) -> Result<usize> {
        let alias = model_alias.trim();
        let profile = self
            .list_llms()
            .into_iter()
            .find(|(_, _, cur)| *cur)
            .and_then(|(_, name, _)| name.split(':').next().map(str::to_string))
            .context("no active LLM profile")?;
        self.next_llm_by_name(&format!("{profile}:{alias}"))
    }
    pub fn history_info(&self) -> Vec<String> {
        self.history_info.lock().clone()
    }
    pub fn message_count(&self) -> usize {
        self.messages.lock().len()
    }
    pub fn restore_messages(&self, messages: Vec<ChatMessage>) {
        *self.messages.lock() = messages;
        self.history_info.lock().clear();
    }
    pub fn restore_session_snapshot(&self, history_info: Vec<String>, messages: Vec<ChatMessage>) {
        *self.messages.lock() = messages;
        *self.history_info.lock() = history_info;
    }
    pub fn fork_session(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            llm: Arc::clone(&self.llm),
            tools: Arc::clone(&self.tools),
            history_info: Arc::new(Mutex::new(self.history_info.lock().clone())),
            key_info: Arc::new(Mutex::new(self.key_info.lock().clone())),
            related_sop: Arc::new(Mutex::new(self.related_sop.lock().clone())),
            plan_path: Arc::new(Mutex::new(self.plan_path.lock().clone())),
            done_hooks: Arc::new(Mutex::new(self.done_hooks.lock().clone())),
            messages: Arc::new(Mutex::new(self.messages.lock().clone())),
            session_overrides: Arc::new(Mutex::new(self.session_overrides.lock().clone())),
            stop: Arc::new(AtomicBool::new(false)),
            is_running: Arc::new(AtomicBool::new(false)),
            task_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
    pub fn rewind_user_turns(&self, n: usize) -> Result<usize> {
        if n == 0 {
            bail!("rewind count must be >= 1");
        }
        let mut messages = self.messages.lock();
        let user_positions = messages
            .iter()
            .enumerate()
            .filter_map(|(idx, msg)| (msg.role == "user").then_some(idx))
            .collect::<Vec<_>>();
        if n > user_positions.len() {
            bail!("only {} user turns are available", user_positions.len());
        }
        let cut_at = user_positions[user_positions.len() - n];
        let removed = messages.len().saturating_sub(cut_at);
        messages.truncate(cut_at);
        self.history_info
            .lock()
            .push(format!("[USER]: /rewind {n}"));
        Ok(removed)
    }

    /// Set a session override value that persists for the duration of the session
    pub fn set_session_override(&self, key: &str, value: &str) {
        self.session_overrides
            .lock()
            .insert(key.to_string(), Value::String(value.to_string()));
    }

    pub async fn put_task(&self, raw_query: impl Into<String>) -> Result<String> {
        self.put_task_with_events(raw_query, |_| {}).await
    }

    pub async fn put_task_with_events(
        &self,
        raw_query: impl Into<String>,
        emit: impl Fn(AgentEvent) + Send + Sync,
    ) -> Result<String> {
        let raw_query = raw_query.into();
        if let Some(out) = self.handle_slash_cmd(&raw_query).await? {
            emit(AgentEvent::SlashOutput {
                content: out.clone(),
            });
            return Ok(out);
        }
        self.run_once(raw_query, &emit).await
    }

    async fn handle_slash_cmd(&self, raw_query: &str) -> Result<Option<String>> {
        let trimmed = raw_query.trim();
        if !trimmed.starts_with('/') {
            return Ok(None);
        }
        if trimmed == "/resume" {
            return Ok(Some(format_continue_list(&self.cfg, 10)?));
        }
        if trimmed == "/help" {
            return Ok(Some(
                "命令列表:\n\
                 /stop - 停止当前任务\n\
                 /status - 查看状态\n\
                 /llm 或 /llms - 查看当前模型列表\n\
                 /llm <n|profile|profile:model> - 按序号、profile 或 profile:model 切换模型\n\
                 /models - 查看当前 profile 下的模型\n\
                 /model <name> - 在当前 profile 内切换模型\n\
                 /continue - 列出可恢复会话\n\
                 /continue <n> - 恢复第 n 个会话摘要\n\
                 /new - 开启新对话并清空当前上下文\n\
                 /btw <问题> - 基于当前上下文做临时侧问"
                    .into(),
            ));
        }
        if trimmed == "/status" {
            let llms = self
                .list_llms()
                .into_iter()
                .map(|(i, n, cur)| format!("{} [{i}] {n}", if cur { "->" } else { "  " }))
                .collect::<Vec<_>>()
                .join("\n");
            let state = if self.is_running.load(Ordering::SeqCst) {
                "运行中"
            } else {
                "可接收任务"
            };
            return Ok(Some(format!("状态: {state}\nLLMs:\n{llms}")));
        }
        if trimmed == "/stop" {
            self.abort();
            return Ok(Some("⏹️ 正在停止当前任务".into()));
        }
        if trimmed == "/btw" || trimmed == "/btw help" || trimmed == "/btw ?" {
            return Ok(Some(
                "**/btw 用法**：`/btw <你的问题>`，基于当前上下文做一次性侧问，不写入主会话历史。"
                    .into(),
            ));
        }
        if let Some(question) = trimmed.strip_prefix("/btw ") {
            return Ok(Some(self.side_question(question.trim()).await?));
        }
        if trimmed == "/continue" {
            return Ok(Some(format_continue_list(&self.cfg, 20)?));
        }
        if let Some(idx) = trimmed
            .strip_prefix("/continue ")
            .and_then(|s| s.trim().parse::<usize>().ok())
        {
            return Ok(Some(restore_continue_summary(self, idx)?));
        }
        if trimmed == "/llms" || trimmed == "/llm" {
            let lines = self
                .list_llms()
                .into_iter()
                .map(|(i, n, cur)| format!("{} [{i}] {n}", if cur { "->" } else { "  " }))
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(Some(format!("LLMs:\n{lines}")));
        }
        if trimmed == "/models" {
            let llms = self.list_llms();
            let profile = llms
                .iter()
                .find(|(_, _, cur)| *cur)
                .and_then(|(_, name, _)| name.split(':').next())
                .unwrap_or("")
                .to_string();
            let lines = llms
                .into_iter()
                .filter(|(_, name, _)| name.starts_with(&format!("{profile}:")))
                .map(|(i, n, cur)| {
                    let alias = n
                        .split_once(':')
                        .and_then(|(_, rest)| rest.split_whitespace().next())
                        .unwrap_or(n.as_str());
                    format!("{} [{i}] {alias} - {n}", if cur { "->" } else { "  " })
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(Some(format!("Models for {profile}:\n{lines}")));
        }
        if let Some(target) = trimmed.strip_prefix("/llm ").map(str::trim) {
            if let Ok(n) = target.parse::<usize>() {
                self.next_llm(n)?;
                return Ok(Some(format!("✅ 已切换到模型 #{n}")));
            }
            let n = self.next_llm_by_name(target)?;
            return Ok(Some(format!("✅ 已切换到模型 `{target}` (#{n})")));
        }
        if let Some(target) = trimmed.strip_prefix("/model ").map(str::trim) {
            let n = self.next_llm_model_by_name(target)?;
            return Ok(Some(format!(
                "✅ 已切换到当前 profile 模型 `{target}` (#{n})"
            )));
        }
        if let Some((key, value)) = trimmed
            .strip_prefix("/session.")
            .and_then(|s| s.split_once('='))
        {
            let key = key.trim();
            let mut raw = value.trim().to_string();
            let value_file = self.cfg.temp_dir.join(&raw);
            if value_file.is_file() {
                raw = fs::read_to_string(&value_file)
                    .with_context(|| format!("read session value file {}", value_file.display()))?
                    .trim()
                    .to_string();
            }
            let parsed = serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw));
            self.session_overrides
                .lock()
                .insert(key.to_string(), parsed.clone());
            let applied_to_llm = self.llm.set_session_option(key, &parsed)?;
            return Ok(Some(smart_format(
                &format!(
                    "✅ session.{key} = {parsed}{}",
                    if applied_to_llm {
                        " (LLM backend updated)"
                    } else {
                        ""
                    }
                ),
                500,
                " ... ",
            )));
        }
        if trimmed == "/new" {
            self.messages.lock().clear();
            self.history_info.lock().clear();
            self.key_info.lock().clear();
            self.related_sop.lock().clear();
            *self.plan_path.lock() = None;
            self.done_hooks.lock().clear();
            self.session_overrides.lock().clear();
            return Ok(Some("✅ 已开始新会话".into()));
        }
        Ok(None)
    }

    fn working_memory_prompt(&self, turn: usize) -> String {
        let history = self.history_info.lock().clone();
        let key_info = self.key_info.lock().clone();
        let related_sop = self.related_sop.lock().clone();
        let plan_path = self.plan_path.lock().clone();
        let window = 30;
        let earlier = if history.len() > window {
            let folded = fold_earlier_history(&history[..history.len() - window]);
            format!("<earlier_context>\n{folded}\n</earlier_context>\n")
        } else {
            String::new()
        };
        let recent = if history.len() > window {
            history[history.len() - window..].join("\n")
        } else {
            history.join("\n")
        };
        let mut prompt = format!(
            "\n### [WORKING MEMORY]\n{earlier}<history>\n{recent}\n</history>\nCurrent turn: {turn}\n"
        );
        if !key_info.trim().is_empty() {
            prompt.push_str(&format!("\n<key_info>{key_info}</key_info>"));
        }
        if !related_sop.trim().is_empty() {
            prompt.push_str(&format!("\n{related_sop}"));
        }
        if let Some(plan) = plan_path {
            prompt.push_str(&format!("\n<plan_mode>{}</plan_mode>", plan.display()));
        }
        prompt
    }

    fn apply_turn_end_injections(&self, turn: usize, next_prompt: &mut String) {
        self.consume_runtime_control_files();
        let in_plan = self.plan_path.lock().is_some();
        if turn.is_multiple_of(65) && !in_plan {
            next_prompt.push_str(&format!(
                "\n\n[DANGER] 已连续执行第 {turn} 轮。必须总结情况进行ask_user，不允许继续重试。"
            ));
        } else if turn.is_multiple_of(7) {
            next_prompt.push_str(&format!(
                "\n\n[DANGER] 已连续执行第 {turn} 轮。禁止无效重试。若无有效进展，必须切换策略：1. 探测物理边界 2. 请求用户协助。如有需要，可调用 update_working_checkpoint 保存关键上下文。"
            ));
        } else if turn.is_multiple_of(10) {
            next_prompt.push_str(&global_memory_prompt(&self.cfg));
        }
        if let Some(plan) = self.plan_path.lock().clone() {
            if turn >= 10 && turn.is_multiple_of(5) {
                next_prompt.insert_str(
                    0,
                    &format!(
                        "[Plan Hint] 正在计划模式。必须 file_read({}) 确认当前步骤，回复开头引用：📌 当前步骤：...\n\n",
                        plan.display()
                    ),
                );
            }
            if turn >= 90 {
                next_prompt.push_str(&format!(
                    "\n\n[DANGER] Plan模式已运行 {turn} 轮，已达上限。必须 ask_user 汇报进度并确认是否继续。"
                ));
            }
        }
        if let Some(injected) = consume_file(&self.cfg.temp_dir, "_intervene") {
            next_prompt.push_str(&format!("\n\n[MASTER] {injected}\n"));
        }
        next_prompt.push_str(&self.working_memory_prompt(turn));
    }

    fn has_seen_tool_call(&self, tool_name: &str) -> bool {
        self.messages.lock().iter().any(|msg| {
            msg.content
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| {
                    calls
                        .iter()
                        .any(|call| call.get("name").and_then(Value::as_str) == Some(tool_name))
                })
        })
    }

    fn consume_runtime_control_files(&self) {
        if let Some(injected) = consume_file(&self.cfg.temp_dir, "_keyinfo") {
            *self.key_info.lock() = format!("[MASTER] {injected}");
        }
        if let Some(injected) = consume_file(&self.cfg.temp_dir, "_related_sop") {
            *self.related_sop.lock() = injected;
        }
        if let Some(plan) = consume_file(&self.cfg.temp_dir, "_plan_mode") {
            let p = Path::new(plan.trim());
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.cfg.temp_dir.join(p)
            };
            *self.plan_path.lock() = Some(abs);
        }
        if consume_file(&self.cfg.temp_dir, "_exit_plan_mode").is_some() {
            *self.plan_path.lock() = None;
        }
        if let Some(hook) = consume_file(&self.cfg.temp_dir, "_done_hook") {
            self.done_hooks.lock().push_back(hook);
        }
        if let Some(hooks) = consume_file(&self.cfg.temp_dir, "_done_hooks") {
            for line in hooks.lines().map(str::trim).filter(|s| !s.is_empty()) {
                self.done_hooks.lock().push_back(line.to_string());
            }
        }
    }

    fn plan_remaining_items(&self) -> Option<usize> {
        let path = self.plan_path.lock().clone()?;
        let text = fs::read_to_string(path).ok()?;
        Some(text.matches("[ ]").count())
    }

    fn plan_completion_intercept(&self, response: &AgentResponse) -> Option<String> {
        self.plan_path.lock().as_ref()?;
        let content = response.content.as_str();
        let claims_done = ["任务完成", "全部完成", "已完成所有", "🏁"]
            .iter()
            .any(|kw| content.contains(kw));
        let has_verify = ["VERDICT", "[VERIFY]", "验证subagent"]
            .iter()
            .any(|kw| content.contains(kw));
        (claims_done && !has_verify).then(|| {
            "⛔ [验证拦截] 检测到你在plan模式下声称完成，但未执行[VERIFY]验证步骤。请先按plan_sop §四启动验证subagent，获得VERDICT后才能声称完成。".to_string()
        })
    }

    async fn side_question(&self, question: &str) -> Result<String> {
        let mut snapshot = self.messages.lock().clone();
        snapshot.push(ChatMessage::text(
            "user",
            format!(
                "<system-reminder>\n这是用户的临时插问(side question)。主 agent 不应被打断；请只基于已有上下文一次性回答。没有工具可用，信息不足就坦白说明。\n</system-reminder>\n\n{question}"
            ),
        ));
        let response = self.llm.chat(&snapshot, &Value::Array(vec![])).await?;
        Ok(format!(
            "> 🟡 /btw {question}\n\n{}",
            if response.content.trim().is_empty() {
                "*(空回复)*"
            } else {
                response.content.trim()
            }
        ))
    }

    async fn run_once(
        &self,
        user_input: String,
        emit: &(impl Fn(AgentEvent) + Send + Sync),
    ) -> Result<String> {
        let _task_guard = self.task_lock.lock().await;
        self.is_running.store(true, Ordering::SeqCst);
        let run_result = self.run_once_inner(user_input, emit).await;
        self.is_running.store(false, Ordering::SeqCst);
        run_result.map(finalize_agent_output)
    }

    async fn run_once_inner(
        &self,
        user_input: String,
        emit: &(impl Fn(AgentEvent) + Send + Sync),
    ) -> Result<String> {
        self.stop.store(false, Ordering::SeqCst);
        let _ = fs::remove_file(self.cfg.temp_dir.join("_stop_signal"));
        self.history_info.lock().push(format!(
            "[USER]: {}",
            smart_format(&user_input.replace('\n', " "), 200, " ... ")
        ));
        let mut out = String::new();
        let tools_schema = load_tool_schema(&self.cfg.resource_dir, "")?;
        let mut sys = system_prompt(&self.cfg.resource_dir)?;
        sys.push_str(&resource_memory_prompt(&self.cfg));
        let (extra_sys, peer_hint) = {
            let overrides = self.session_overrides.lock();
            (
                overrides
                    .get("extra_sys_prompt")
                    .or_else(|| overrides.get("system"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                overrides
                    .get("peer_hint")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            )
        };
        if let Some(extra) = extra_sys {
            sys.push_str(&extra);
        }
        if peer_hint {
            sys.push_str("\n[Peer] 用户提及其他会话/后台任务状态时: temp/model_responses/ (只找近期修改的文件尾部)\n");
        }
        if self.messages.lock().is_empty() {
            self.messages.lock().push(ChatMessage::text("system", sys));
        }
        let user_input = with_runtime_recall_prompt(&self.cfg, &user_input);
        self.messages
            .lock()
            .push(ChatMessage::text("user", user_input));
        let prompt_snapshot = {
            let guard = self.messages.lock();
            snapshot_prompt(&guard)
        };
        append_model_log(&self.cfg, "Prompt", &prompt_snapshot)?;
        let mut long_term_reminded = false;
        let mut consecutive_blank_turns = 0u32;

        for turn in 1..=self.cfg.max_turns {
            if consume_file(&self.cfg.temp_dir, "_stop").is_some() {
                self.abort();
            }
            if self.stop.load(Ordering::SeqCst) {
                out.push_str("\n[Stopped] 用户强制终止\n");
                emit(AgentEvent::Stopped);
                break;
            }
            emit(AgentEvent::TurnStarted { turn });
            out.push_str(&format!("\n\n**LLM Running (Turn {turn}) ...**\n\n"));
            let snapshot = self.messages.lock().clone();
            let saw_content_delta = Arc::new(AtomicBool::new(false));
            let saw_thinking_delta = Arc::new(AtomicBool::new(false));
            let stream_content_flag = Arc::clone(&saw_content_delta);
            let stream_thinking_flag = Arc::clone(&saw_thinking_delta);
            let response = self
                .llm
                .chat_with_events(
                    &snapshot,
                    &tools_schema,
                    &|event| match event {
                        LlmStreamEvent::ContentDelta { content } => {
                            if !content.is_empty() {
                                stream_content_flag.store(true, Ordering::SeqCst);
                                emit(AgentEvent::AssistantMessageDelta { turn, content });
                            }
                        }
                        LlmStreamEvent::ThinkingDelta { content } => {
                            if !content.is_empty() {
                                stream_thinking_flag.store(true, Ordering::SeqCst);
                                emit(AgentEvent::ThinkingMessageDelta { turn, content });
                            }
                        }
                        LlmStreamEvent::Usage { usage } => {
                            emit(AgentEvent::LlmUsage { turn, usage });
                        }
                    },
                    Some(&self.stop),
                )
                .await?;
            append_model_log(&self.cfg, "Response", &serde_json::to_string(&response)?)?;
            emit_response_annotations(
                turn,
                &response,
                saw_thinking_delta.load(Ordering::SeqCst),
                emit,
            );
            let display_content = display_content_with_summary(&response.content);
            let assistant_content = content_without_summary(&response.content);
            if !display_content.trim().is_empty() {
                out.push_str(&display_content);
                out.push('\n');
                if !saw_content_delta.load(Ordering::SeqCst) && !assistant_content.is_empty() {
                    emit(AgentEvent::AssistantMessage {
                        turn,
                        content: assistant_content,
                    });
                }
            }
            self.messages.lock().push(ChatMessage {
                role: "assistant".into(),
                content: json!({"text": response.content, "thinking": response.thinking, "tool_calls": response.tool_calls, "raw": response.raw}),
            });
            if response.tool_calls.is_empty() {
                let summary = fallback_turn_summary(&response, &[]);
                self.history_info.lock().push(format!("[Agent] {summary}"));
                self.consume_runtime_control_files();
                if let Some(next_prompt) = self.plan_completion_intercept(&response) {
                    out.push_str("\n[Warn] Plan completion claim intercepted.\n");
                    self.messages.lock().push(ChatMessage {
                        role: "user".into(),
                        content: json!({"content": next_prompt, "tool_results": []}),
                    });
                    continue;
                }
                if let Some(next_prompt) = no_tool_next_prompt(&response) {
                    consecutive_blank_turns += 1;
                    if consecutive_blank_turns >= 3 {
                        out.push_str(&format!("\n[Error] 连续 {} 轮空响应/无工具调用，疑似第三方API解析异常，强制终止循环。response.content前200字: {}\n", consecutive_blank_turns, &response.content.chars().take(200).collect::<String>()));
                        break;
                    }
                    out.push_str("\n[Info] No-tool response requires another turn.\n");
                    self.messages.lock().push(ChatMessage {
                        role: "user".into(),
                        content: json!({"content": next_prompt, "tool_results": []}),
                    });
                    continue;
                }
                if turn >= 15
                    && !long_term_reminded
                    && !self.has_seen_tool_call("start_long_term_update")
                {
                    long_term_reminded = true;
                    out.push_str("\n[Info] Long task settlement required before final answer.\n");
                    self.messages.lock().push(ChatMessage {
                        role: "user".into(),
                        content: json!({
                            "content": "[SYSTEM] 当前任务已经执行15轮以上。若有任何经工具验证、未来可复用的环境事实/用户偏好/踩坑经验，必须先调用 start_long_term_update 进行长期记忆结算；若确实没有可记忆内容，请用一句话明确说明没有长期记忆增量，然后再给最终答复。",
                            "tool_results": []
                        }),
                    });
                    continue;
                }
                if self.plan_path.lock().is_some()
                    && let Some(0) = self.plan_remaining_items()
                {
                    *self.plan_path.lock() = None;
                    out.push_str("\n[Info] Plan完成：plan.md中0个[ ]残留，退出plan模式。\n");
                }
                emit(AgentEvent::TurnFinished {
                    turn,
                    stop_reason: "end_turn".into(),
                });
                break;
            }
            consecutive_blank_turns = 0;
            let mut next_prompts = Vec::new();
            let mut tool_results = Vec::new();
            for (idx, tc) in response.tool_calls.iter().enumerate() {
                if self.stop.load(Ordering::SeqCst) {
                    for skipped in response.tool_calls.iter().skip(idx) {
                        tool_results.push(ToolResult {
                            tool_call_id: skipped.id.clone(),
                            name: skipped.name.clone(),
                            content: json!({"status":"skipped","msg":"stop requested"}),
                        });
                    }
                    break;
                }
                out.push_str(&format!("🛠️ Tool: `{}` args: {}\n", tc.name, tc.args));
                if langfuse_trace_enabled(&self.cfg) {
                    append_langfuse_trace(
                        &self.cfg,
                        &tc.name,
                        "tool",
                        "start",
                        json!({"turn":turn,"index":idx,"args":tc.args}),
                    )?;
                }
                emit(AgentEvent::ToolStarted {
                    turn,
                    index: idx,
                    name: tc.name.clone(),
                    args: tc.args.clone(),
                });
                let outcome = self
                    .tools
                    .dispatch(&tc.name, tc.args.clone(), &response, idx)
                    .await?;
                out.push_str(&format!("{}\n", outcome.data));
                if langfuse_trace_enabled(&self.cfg) {
                    append_langfuse_trace(
                        &self.cfg,
                        &tc.name,
                        "tool",
                        "end",
                        json!({"turn":turn,"index":idx,"output":outcome.data}),
                    )?;
                }
                emit(AgentEvent::ToolFinished {
                    turn,
                    index: idx,
                    name: tc.name.clone(),
                    data: outcome.data.clone(),
                });
                tool_results.push(ToolResult {
                    tool_call_id: tc.id.clone(),
                    name: tc.name.clone(),
                    content: outcome.data.clone(),
                });
                if outcome.should_exit {
                    for skipped in response.tool_calls.iter().skip(idx + 1) {
                        tool_results.push(ToolResult {
                            tool_call_id: skipped.id.clone(),
                            name: skipped.name.clone(),
                            content: json!({"status":"skipped","msg":"previous tool requested exit"}),
                        });
                    }
                    self.messages.lock().push(ChatMessage {
                        role: "user".into(),
                        content: json!({"content":"", "tool_results": tool_results}),
                    });
                    emit(AgentEvent::TurnFinished {
                        turn,
                        stop_reason: "tool_exit".into(),
                    });
                    let history = self.history_info.lock().clone();
                    let messages = self.messages.lock().clone();
                    append_history_log(&self.cfg, &history)?;
                    archive_l4_session(&self.cfg, &history, &messages)?;
                    return Ok(out);
                }
                if let Some(p) = outcome.next_prompt
                    && !p.trim().is_empty()
                {
                    next_prompts.push(p);
                }
            }
            if consume_file(&self.cfg.temp_dir, "_stop").is_some() {
                self.messages.lock().push(ChatMessage {
                    role: "user".into(),
                    content: json!({"content":"", "tool_results": tool_results}),
                });
                out.push_str("\n[Stopped] _stop requested at turn end\n");
                emit(AgentEvent::Stopped);
                break;
            }
            self.consume_runtime_control_files();
            if next_prompts.is_empty()
                && let Some(hook) = self.done_hooks.lock().pop_front()
            {
                next_prompts.push(hook);
            }
            let next_prompt = if next_prompts.is_empty() {
                "\n".to_string()
            } else {
                next_prompts.join("\n")
            };
            let mut next_prompt = next_prompt;
            let summary = fallback_turn_summary(&response, &response.tool_calls);
            self.history_info.lock().push(format!("[Agent] {summary}"));
            if !response.content.contains("<summary>") {
                next_prompt.push_str("\n\n\n[SYSTEM] 必须在回复文本中包含<summary>！\n\n");
            }
            self.apply_turn_end_injections(turn, &mut next_prompt);
            self.messages.lock().push(ChatMessage {
                role: "user".into(),
                content: json!({"content": next_prompt, "tool_results": tool_results}),
            });
        }
        let history = self.history_info.lock().clone();
        let messages = self.messages.lock().clone();
        append_history_log(&self.cfg, &history)?;
        archive_l4_session(&self.cfg, &history, &messages)?;
        Ok(out)
    }
}

fn finalize_agent_output(out: String) -> String {
    let out = out.replace("</summary>", "</summary>\n\n");
    wrap_file_content_blocks(&out)
}

fn wrap_file_content_blocks(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let Some(start) = rest.find("<file_content>") else {
            out.push_str(rest);
            break;
        };
        let Some(end_rel) = rest[start..].find("</file_content>") else {
            out.push_str(rest);
            break;
        };
        let end = start + end_rel + "</file_content>".len();
        out.push_str(&rest[..start]);
        out.push_str("\n````\n");
        out.push_str(rest[start..end].trim());
        out.push_str("\n````");
        rest = &rest[end..];
    }
    out
}

fn fold_earlier_history(lines: &[String]) -> String {
    const FALLBACK: &str = "[Agent]";
    let mut parts = Vec::new();
    let mut count = 0usize;
    let mut last = String::new();
    let flush = |parts: &mut Vec<String>, count: &mut usize, last: &mut String| {
        if *count > 0 {
            if last.contains(FALLBACK) {
                parts.push(format!("[Agent]（{} turns）", count));
            } else {
                parts.push(format!("{}（{} turns）", last, count));
            }
        }
        *count = 0;
        last.clear();
    };
    for line in lines {
        if line.starts_with("[USER]") {
            flush(&mut parts, &mut count, &mut last);
            parts.push(line.clone());
        } else {
            count += 1;
            last = line.clone();
        }
    }
    flush(&mut parts, &mut count, &mut last);
    let start = parts.len().saturating_sub(150);
    parts[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn write_resource_markers(dir: &Path) {
        fs::create_dir_all(dir.join("assets")).unwrap();
        fs::write(dir.join("assets/tools_schema.json"), "[]").unwrap();
        fs::write(dir.join("assets/sys_prompt.txt"), "prompt").unwrap();
    }

    #[test]
    fn agent_paths_prefer_explicit_home_workspace_and_resources() {
        let d = tempfile::tempdir().unwrap();
        let current = d.path().join("cwd");
        let home = d.path().join("home");
        let workspace = d.path().join("workspace");
        let resources = d.path().join("resources");
        let paths = resolve_agent_paths_with_options(
            &current,
            AgentPathOptions {
                home_dir: Some(home.clone()),
                workspace_dir: Some(workspace.clone()),
                resource_dir: Some(resources.clone()),
                executable_dir: None,
            },
        );
        assert_eq!(paths.home_dir, home);
        assert_eq!(paths.workspace_dir, workspace);
        assert_eq!(paths.resource_dir, resources);
        assert_eq!(paths.temp_dir, paths.home_dir.join("temp"));
        assert_eq!(paths.memory_dir, paths.home_dir.join("memory"));
        assert_eq!(paths.logs_dir, paths.home_dir.join("logs"));
        assert_eq!(paths.sessions_dir, paths.home_dir.join("sessions"));
        assert_eq!(paths.browser_dir, paths.home_dir.join("browser"));
    }

    #[test]
    fn agent_paths_prefer_packaged_resources_over_source_and_home() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("source");
        let exe = d.path().join("bin");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("Cargo.toml"), "[workspace]").unwrap();
        write_resource_markers(&source);
        write_resource_markers(&exe.join("resources"));
        let paths = resolve_agent_paths_with_options(
            &source,
            AgentPathOptions {
                home_dir: Some(d.path().join("home")),
                executable_dir: Some(exe.clone()),
                ..Default::default()
            },
        );
        assert_eq!(paths.resource_dir, exe.join("resources"));
    }

    #[test]
    fn agent_paths_detect_source_checkout_from_subdirectory() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("source");
        let nested = source.join("crates/koda-agent-core");
        fs::create_dir_all(&nested).unwrap();
        fs::write(source.join("Cargo.toml"), "[workspace]").unwrap();
        write_resource_markers(&source);
        // Temporarily remove env vars that would override path detection
        let saved_ws = env::var_os("KODA_WORKSPACE");
        let saved_home = env::var_os("KODA_AGENT_HOME");
        // SAFETY: test-only env manipulation before calling into tested code
        #[allow(unused_unsafe)]
        unsafe {
            env::remove_var("KODA_WORKSPACE");
            env::remove_var("KODA_AGENT_HOME");
        }
        let paths = resolve_agent_paths_with_options(
            &nested,
            AgentPathOptions {
                home_dir: Some(d.path().join("home")),
                ..Default::default()
            },
        );
        // Restore env vars
        #[allow(unused_unsafe)]
        unsafe {
            if let Some(ws) = saved_ws {
                env::set_var("KODA_WORKSPACE", ws);
            }
            if let Some(h) = saved_home {
                env::set_var("KODA_AGENT_HOME", h);
            }
        }
        assert_eq!(paths.workspace_dir, nested);
        assert_eq!(paths.resource_dir, source);
    }

    #[test]
    fn agent_paths_fall_back_to_home_resources() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let paths = resolve_agent_paths_with_options(
            d.path().join("plain-workspace"),
            AgentPathOptions {
                home_dir: Some(home.clone()),
                ..Default::default()
            },
        );
        assert_eq!(paths.resource_dir, home.join("resources"));
    }

    #[test]
    fn agent_config_from_env_separates_home_workspace_and_resources() {
        let d = tempfile::tempdir().unwrap();
        let current = d.path().join("current");
        let home = d.path().join("home");
        let workspace = d.path().join("workspace");
        let resources = d.path().join("resources");
        fs::create_dir_all(home.join("config")).unwrap();
        fs::write(
            home.join("config/llms.toml"),
            r#"
[default]
base_url = "http://example.invalid/v1"
api_key = "sk-test"
model = "test-model"
"#,
        )
        .unwrap();
        let cfg = AgentConfig::from_env_with_path_options(
            &current,
            AgentPathOptions {
                home_dir: Some(home.clone()),
                workspace_dir: Some(workspace.clone()),
                resource_dir: Some(resources.clone()),
                executable_dir: None,
            },
        )
        .unwrap();
        assert_eq!(cfg.home_dir, home);
        assert_eq!(cfg.workspace_dir, workspace);
        assert_eq!(cfg.resource_dir, resources);
        assert_eq!(cfg.root_dir, cfg.resource_dir);
        assert_eq!(cfg.temp_dir, cfg.home_dir.join("temp"));
        assert_eq!(cfg.memory_dir, cfg.home_dir.join("memory"));
        assert_eq!(cfg.logs_dir, cfg.home_dir.join("logs"));
    }

    #[test]
    fn agent_config_loads_profile_toml_without_openai_env() {
        let _guard = env_lock();
        unsafe {
            env::remove_var("OPENAI_BASE_URL");
            env::remove_var("OPENAI_API_KEY");
            env::remove_var("OPENAI_MODEL");
            env::remove_var("KODA_LLM_PROFILE");
            env::remove_var("KODA_LLM_MODEL");
            env::set_var("KODA_TEST_MIMO_KEY", "sk-mimo-test");
        }
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        fs::create_dir_all(home.join("config")).unwrap();
        fs::write(
            home.join("config/llms.toml"),
            r#"
[selector]
default_profile = "mimo"
default_model = "pro"

[defaults]
timeout_secs = 123

[[profiles]]
name = "mimo"
kind = "native_oai"
base_url = "https://api.xiaomimimo.com/v1"
api_key_env = "KODA_TEST_MIMO_KEY"
auth_scheme = "header"
auth_header = "api-key"
[[profiles.models]]
name = "pro"
id = "mimo-v2.5"
"#,
        )
        .unwrap();
        let cfg = AgentConfig::from_env_with_path_options(
            d.path().join("workspace"),
            AgentPathOptions {
                home_dir: Some(home),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.openai_model, "mimo-v2.5");
        assert_eq!(cfg.openai_api_key, "sk-mimo-test");
        assert_eq!(cfg.openai_base_url, "https://api.xiaomimimo.com/v1");
        assert_eq!(cfg.auth_scheme.as_deref(), Some("header"));
        assert_eq!(cfg.auth_header.as_deref(), Some("api-key"));
        assert_eq!(cfg.timeout_secs, 123);
        unsafe {
            env::remove_var("KODA_TEST_MIMO_KEY");
        }
    }

    #[test]
    fn agent_config_selects_profile_from_koda_llm_profile() {
        let _guard = env_lock();
        unsafe {
            env::remove_var("OPENAI_BASE_URL");
            env::remove_var("OPENAI_API_KEY");
            env::remove_var("OPENAI_MODEL");
            env::set_var("KODA_LLM_PROFILE", "backup");
            env::set_var("KODA_TEST_PRIMARY_KEY", "sk-primary-test");
            env::set_var("KODA_TEST_BACKUP_KEY", "sk-backup-test");
        }
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        fs::create_dir_all(home.join("config")).unwrap();
        fs::write(
            home.join("config/llms.toml"),
            r#"
[selector]
default_profile = "primary"
default_model = "default"

[[profiles]]
name = "primary"
base_url = "http://primary"
api_key_env = "KODA_TEST_PRIMARY_KEY"
[[profiles.models]]
name = "default"
id = "primary-model"

[[profiles]]
name = "backup"
base_url = "http://backup"
api_key_env = "KODA_TEST_BACKUP_KEY"
api_mode = "responses"

[[profiles.models]]
name = "default"
id = "backup-model"
"#,
        )
        .unwrap();
        let cfg = AgentConfig::from_env_with_path_options(
            d.path().join("workspace"),
            AgentPathOptions {
                home_dir: Some(home),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.openai_model, "backup-model");
        assert_eq!(cfg.llm_api_style, "responses");
        assert_eq!(cfg.openai_api_key, "sk-backup-test");
        assert_eq!(cfg.llm_configs[0].name, "backup:default");
        assert_eq!(cfg.llm_configs[1].name, "primary:default");
        unsafe {
            env::remove_var("KODA_LLM_PROFILE");
            env::remove_var("KODA_LLM_MODEL");
            env::remove_var("KODA_TEST_PRIMARY_KEY");
            env::remove_var("KODA_TEST_BACKUP_KEY");
        }
    }

    #[test]
    fn agent_config_profile_override_uses_that_profiles_first_model() {
        let _guard = env_lock();
        unsafe {
            env::remove_var("OPENAI_BASE_URL");
            env::remove_var("OPENAI_API_KEY");
            env::remove_var("OPENAI_MODEL");
            env::set_var("KODA_LLM_PROFILE", "backup");
            env::remove_var("KODA_LLM_MODEL");
            env::set_var("KODA_TEST_PRIMARY_KEY", "sk-primary-test");
            env::set_var("KODA_TEST_BACKUP_KEY", "sk-backup-test");
        }
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        fs::create_dir_all(home.join("config")).unwrap();
        fs::write(
            home.join("config/llms.toml"),
            r#"
[selector]
default_profile = "primary"
default_model = "pro"

[[profiles]]
name = "primary"
base_url = "http://primary"
api_key_env = "KODA_TEST_PRIMARY_KEY"
[[profiles.models]]
name = "pro"
id = "primary-pro"

[[profiles]]
name = "backup"
base_url = "http://backup"
api_key_env = "KODA_TEST_BACKUP_KEY"
[[profiles.models]]
name = "default"
id = "backup-default"
"#,
        )
        .unwrap();
        let cfg = AgentConfig::from_env_with_path_options(
            d.path().join("workspace"),
            AgentPathOptions {
                home_dir: Some(home),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.openai_model, "backup-default");
        assert_eq!(cfg.llm_configs[0].name, "backup:default");
        unsafe {
            env::remove_var("KODA_LLM_PROFILE");
            env::remove_var("KODA_LLM_MODEL");
            env::remove_var("KODA_TEST_PRIMARY_KEY");
            env::remove_var("KODA_TEST_BACKUP_KEY");
        }
    }

    #[test]
    fn agent_config_profile_missing_secret_points_to_config_secret() {
        let _guard = env_lock();
        unsafe {
            env::remove_var("OPENAI_BASE_URL");
            env::remove_var("OPENAI_API_KEY");
            env::remove_var("OPENAI_MODEL");
            env::remove_var("KODA_LLM_PROFILE");
            env::remove_var("KODA_LLM_MODEL");
            env::remove_var("KODA_TEST_MISSING_KEY");
        }
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        fs::create_dir_all(home.join("config")).unwrap();
        fs::write(
            home.join("config/llms.toml"),
            r#"
[[profiles]]
name = "mimo"
base_url = "http://mimo"
api_key_env = "KODA_TEST_MISSING_KEY"
[[profiles.models]]
name = "default"
id = "mimo"
"#,
        )
        .unwrap();
        let err = AgentConfig::from_env_with_path_options(
            d.path().join("workspace"),
            AgentPathOptions {
                home_dir: Some(home),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("koda-agent config secret KODA_TEST_MISSING_KEY"));
    }

    #[test]
    fn agent_config_rejects_legacy_openai_env_without_llms_toml() {
        let _guard = env_lock();
        unsafe {
            env::set_var("OPENAI_BASE_URL", "http://legacy");
            env::set_var("OPENAI_API_KEY", "sk-legacy");
            env::set_var("OPENAI_MODEL", "legacy-model");
            env::remove_var("KODA_LLM_PROFILE");
            env::remove_var("KODA_LLM_MODEL");
        }
        let d = tempfile::tempdir().unwrap();
        let err = AgentConfig::from_env_with_path_options(
            d.path().join("workspace"),
            AgentPathOptions {
                home_dir: Some(d.path().join("home")),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("koda-agent config migrate"));
        unsafe {
            env::remove_var("OPENAI_BASE_URL");
            env::remove_var("OPENAI_API_KEY");
            env::remove_var("OPENAI_MODEL");
        }
    }

    struct TestSwitchLlm {
        current: Mutex<usize>,
    }

    #[async_trait]
    impl LlmClient for TestSwitchLlm {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools_schema: &Value,
        ) -> Result<AgentResponse> {
            Ok(AgentResponse {
                thinking: String::new(),
                content: String::new(),
                tool_calls: Vec::new(),
                raw: json!(null),
            })
        }
        fn name(&self) -> String {
            "primary".into()
        }
        fn list_llms(&self) -> Vec<(usize, String, bool)> {
            let current = *self.current.lock().unwrap();
            vec![
                (0, "mimo:pro (mimo-v2.5)".into(), current == 0),
                (1, "deepseek:flash (deepseek-v4-flash)".into(), current == 1),
                (2, "deepseek:pro (deepseek-v4-pro)".into(), current == 2),
            ]
        }
        fn switch_llm(&self, n: usize) -> Result<()> {
            if n > 2 {
                bail!("bad index");
            }
            *self.current.lock().unwrap() = n;
            Ok(())
        }
    }

    struct NoopTools;

    #[async_trait]
    impl ToolDispatcher for NoopTools {
        async fn dispatch(
            &self,
            _name: &str,
            _args: Value,
            _response: &AgentResponse,
            _index: usize,
        ) -> Result<StepOutcome> {
            Ok(StepOutcome::done(json!(null)))
        }
    }

    #[test]
    fn runtime_switches_llm_by_profile_name() {
        let d = tempfile::tempdir().unwrap();
        let cfg = AgentConfig {
            home_dir: d.path().into(),
            workspace_dir: d.path().into(),
            resource_dir: d.path().into(),
            root_dir: d.path().into(),
            temp_dir: d.path().join("temp"),
            memory_dir: d.path().join("memory"),
            logs_dir: d.path().join("logs"),
            sessions_dir: d.path().join("sessions"),
            browser_dir: d.path().join("browser"),
            openai_base_url: "http://x".into(),
            openai_api_key: "sk-test".into(),
            openai_model: "m".into(),
            llm_api_style: "chat".into(),
            auth_scheme: None,
            auth_header: None,
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
            custom_headers: BTreeMap::new(),
            mixin: MixinConfig::default(),
            llm_configs: vec![],
        };
        let rt = AgentRuntime::new(
            cfg,
            Arc::new(TestSwitchLlm {
                current: Mutex::new(0),
            }),
            Arc::new(NoopTools),
        )
        .unwrap();
        assert_eq!(rt.next_llm_by_name("deepseek").unwrap(), 1);
        assert!(rt.list_llms()[1].2);
        assert_eq!(rt.next_llm_by_name("mimo:pro").unwrap(), 0);
        assert!(rt.list_llms()[0].2);
    }

    #[tokio::test]
    async fn runtime_slash_switches_llm_by_profile_model_and_model_alias() {
        let d = tempfile::tempdir().unwrap();
        let cfg = AgentConfig {
            home_dir: d.path().into(),
            workspace_dir: d.path().into(),
            resource_dir: d.path().into(),
            root_dir: d.path().into(),
            temp_dir: d.path().join("temp"),
            memory_dir: d.path().join("memory"),
            logs_dir: d.path().join("logs"),
            sessions_dir: d.path().join("sessions"),
            browser_dir: d.path().join("browser"),
            openai_base_url: "http://x".into(),
            openai_api_key: "sk-test".into(),
            openai_model: "m".into(),
            llm_api_style: "chat".into(),
            auth_scheme: None,
            auth_header: None,
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
            custom_headers: BTreeMap::new(),
            mixin: MixinConfig::default(),
            llm_configs: vec![],
        };
        let rt = AgentRuntime::new(
            cfg,
            Arc::new(TestSwitchLlm {
                current: Mutex::new(0),
            }),
            Arc::new(NoopTools),
        )
        .unwrap();
        let out = rt.put_task("/llm deepseek:flash").await.unwrap();
        assert!(out.contains("deepseek:flash"));
        assert!(rt.list_llms()[1].2);
        let out = rt.put_task("/model pro").await.unwrap();
        assert!(out.contains("pro"));
        assert!(rt.list_llms()[2].2);
    }

    #[test]
    fn url_building_matches_openai_compat() {
        // bare base → auto-insert /v1
        assert_eq!(
            auto_make_url("http://x:1", "chat/completions"),
            "http://x:1/v1/chat/completions"
        );
        // already has /v1
        assert_eq!(
            auto_make_url("http://x:1/v1", "chat/completions"),
            "http://x:1/v1/chat/completions"
        );
        // already contains path → no double
        assert_eq!(
            auto_make_url("http://x:1/v1/chat/completions", "chat/completions"),
            "http://x:1/v1/chat/completions"
        );
        // ZhipuAI /v4 path — must NOT inject /v1 (#2)
        assert_eq!(
            auto_make_url("https://open.bigmodel.cn/api/paas/v4", "chat/completions"),
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
        // other /vN providers
        assert_eq!(
            auto_make_url("http://localhost:8080/v2", "chat/completions"),
            "http://localhost:8080/v2/chat/completions"
        );
        // non-chat path → no /v1 injection regardless
        assert_eq!(
            auto_make_url("http://x:1", "audio/speech"),
            "http://x:1/audio/speech"
        );
    }

    #[test]
    fn smart_format_is_unicode_safe() {
        let text = "<summary>用户问自我进化能力 → 需区分核心模型进化与工具层自我改进 → 后者可实现</summary>";
        let formatted = smart_format(text, 30, "...");
        assert!(formatted.contains("..."));
        assert!(formatted.starts_with("<summary>"));
    }

    #[test]
    fn redact_secret_is_unicode_safe() {
        assert_eq!(redact_secret("密钥abcdef尾巴"), "密钥ab...ef尾巴");
    }

    #[test]
    fn resource_memory_prompt_lists_static_sop_files() {
        let d = tempfile::tempdir().unwrap();
        let resource = d.path().join("resources");
        fs::create_dir_all(resource.join("memory")).unwrap();
        fs::write(resource.join("memory/plan_sop.md"), "plan").unwrap();
        fs::write(resource.join("memory/vision_api.py"), "vision").unwrap();
        let cfg = AgentConfig {
            home_dir: d.path().join("home"),
            workspace_dir: d.path().join("workspace"),
            resource_dir: resource.clone(),
            root_dir: resource,
            temp_dir: d.path().join("temp"),
            memory_dir: d.path().join("home/memory"),
            logs_dir: d.path().join("logs"),
            sessions_dir: d.path().join("sessions"),
            browser_dir: d.path().join("browser"),
            openai_base_url: "http://x".into(),
            openai_api_key: "sk-test".into(),
            openai_model: "m".into(),
            llm_api_style: "chat".into(),
            auth_scheme: None,
            auth_header: None,
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
            custom_headers: BTreeMap::new(),
            mixin: MixinConfig::default(),
            llm_configs: vec![],
        };
        let prompt = resource_memory_prompt(&cfg);
        assert!(prompt.contains("plan_sop.md"));
        assert!(prompt.contains("vision_api.py"));
        assert!(prompt.contains("静态 SOP/helper"));
    }

    #[test]
    fn continue_preview_matches_upstream_summary_and_md_escape() {
        let response =
            r#"[{'type':'text','text':'<summary>修复 README_[demo] 星号*预览</summary>\n正文'}]"#;
        assert_eq!(
            last_response_summary(response).unwrap(),
            "修复 README_[demo] 星号*预览"
        );
        assert_eq!(escape_md("README_[demo]*"), "README\\_\\[demo\\]\\*");
        let prompt = r#"{"role":"user","content":[{"type":"text","text":"继续处理"}]}"#;
        assert_eq!(preview_prompt(prompt), "继续处理");
    }

    #[test]
    fn ensure_dirs_creates_tmwd_cdp_config() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("assets/tmwd_cdp_bridge")).unwrap();
        let cfg = AgentConfig {
            home_dir: d.path().into(),
            workspace_dir: d.path().into(),
            resource_dir: d.path().into(),
            root_dir: d.path().into(),
            temp_dir: d.path().join("temp"),
            memory_dir: d.path().join("memory"),
            logs_dir: d.path().join("logs"),
            sessions_dir: d.path().join("sessions"),
            browser_dir: d.path().join("browser"),
            openai_base_url: "http://x".into(),
            openai_api_key: "sk-test".into(),
            openai_model: "m".into(),
            llm_api_style: "chat".into(),
            auth_scheme: None,
            auth_header: None,
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
            custom_headers: BTreeMap::new(),
            mixin: MixinConfig::default(),
            llm_configs: vec![],
        };
        cfg.ensure_dirs().unwrap();
        let config =
            fs::read_to_string(d.path().join("browser/tmwd_cdp_bridge/config.js")).unwrap();
        assert!(config.starts_with("const TID = '__ljq_"));
        assert!(!d.path().join("assets/tmwd_cdp_bridge/config.js").exists());
    }

    #[test]
    fn model_log_parser_accepts_timestamped_markers_and_l4_archives_history() {
        let d = tempfile::tempdir().unwrap();
        let cfg = AgentConfig {
            home_dir: d.path().into(),
            workspace_dir: d.path().into(),
            resource_dir: d.path().into(),
            root_dir: d.path().into(),
            temp_dir: d.path().join("temp"),
            memory_dir: d.path().join("memory"),
            logs_dir: d.path().join("logs"),
            sessions_dir: d.path().join("sessions"),
            browser_dir: d.path().join("browser"),
            openai_base_url: "http://x".into(),
            openai_api_key: "sk-test".into(),
            openai_model: "m".into(),
            llm_api_style: "chat".into(),
            auth_scheme: None,
            auth_header: None,
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
            custom_headers: BTreeMap::new(),
            mixin: MixinConfig::default(),
            llm_configs: vec![],
        };
        let content = "=== Prompt === 2026-05-10 10:00:00\n[{\"role\":\"user\",\"content\":\"hi\"}]\n=== Response === 2026-05-10 10:00:01\n{\"content\":\"ok\"}\n";
        let pairs = parse_model_log_pairs(content);
        assert_eq!(pairs.len(), 1);
        let history = vec!["[USER]: hi".to_string(), "[Agent] ok".to_string()];
        archive_l4_session(&cfg, &history, &[ChatMessage::text("user", "hi")]).unwrap();
        let all =
            fs::read_to_string(cfg.memory_dir.join("L4_raw_sessions/all_histories.txt")).unwrap();
        assert!(all.contains("[USER]: hi"));
        assert!(
            cfg.memory_dir
                .join("L4_raw_sessions")
                .read_dir()
                .unwrap()
                .count()
                >= 2
        );
    }

    #[test]
    fn runtime_recall_injects_only_for_history_like_queries() {
        let d = tempfile::tempdir().unwrap();
        let cfg = AgentConfig {
            home_dir: d.path().into(),
            workspace_dir: d.path().into(),
            resource_dir: d.path().into(),
            root_dir: d.path().into(),
            temp_dir: d.path().join("temp"),
            memory_dir: d.path().join("memory"),
            logs_dir: d.path().join("logs"),
            sessions_dir: d.path().join("sessions"),
            browser_dir: d.path().join("browser"),
            openai_base_url: "http://x".into(),
            openai_api_key: "sk-test".into(),
            openai_model: "m".into(),
            llm_api_style: "chat".into(),
            auth_scheme: None,
            auth_header: None,
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
            custom_headers: BTreeMap::new(),
            mixin: MixinConfig::default(),
            llm_configs: vec![],
        };
        fs::create_dir_all(cfg.memory_dir.join("L4_raw_sessions")).unwrap();
        fs::write(
            cfg.memory_dir.join("L4_raw_sessions/all_histories.txt"),
            "============================================================\nSESSION: s1\n============================================================\n[USER]: tmwd bridge smoke\n[Agent] fixed contentSettings restore\n",
        )
        .unwrap();

        assert_eq!(
            with_runtime_recall_prompt(&cfg, "tmwd bridge status"),
            "tmwd bridge status"
        );
        let recalled = with_runtime_recall_prompt(&cfg, "继续之前 tmwd bridge 的问题");
        assert!(recalled.contains("<system-recall>"));
        assert!(recalled.contains("contentSettings restore"));
        assert!(recalled.ends_with("继续之前 tmwd bridge 的问题"));
    }

    #[test]
    fn config_loads_multiple_llm_models_from_toml() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("config")).unwrap();
        fs::write(
            d.path().join("config/llms.toml"),
            r#"
[default]
base_url = "http://primary"
api_key = "sk-primary"
model = "primary-model"
api_style = "chat"
stream = false
timeout_secs = 111
connect_timeout = 9
verify = false
thinking_type = "adaptive"

[mixin]
llm_nos = [0, "backup"]
max_retries = 5
base_delay = 0.25
spring_back = 7

[[models]]
name = "backup"
base_url = "http://backup"
api_key = "sk-backup"
model = "backup-model"
api_style = "responses"
stream = true
timeout_secs = 222
connect_timeout_secs = 12
verify_tls = true
reasoning_effort = "high"
thinking_type = "enabled"
thinking_budget_tokens = 32768
max_tokens = 2048
[models.headers]
"x-test" = "yes"
"#,
        )
        .unwrap();
        let primary = LlmModelConfig {
            name: "primary-model".into(),
            base_url: "http://primary".into(),
            api_key: "sk-primary".into(),
            model: "primary-model".into(),
            api_style: "chat".into(),
            auth_scheme: None,
            auth_header: None,
            stream: false,
            timeout_secs: 111,
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
            custom_headers: BTreeMap::new(),
        };
        let configs = load_llm_model_configs(d.path(), &primary, None, &[]);
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].name, "primary-model");
        assert_eq!(configs[1].name, "backup");
        assert_eq!(configs[1].api_style, "responses");
        assert!(configs[1].stream);
        assert_eq!(configs[1].connect_timeout_secs, 12);
        assert!(configs[1].verify_tls);
        assert_eq!(configs[1].reasoning_effort.as_deref(), Some("high"));
        assert_eq!(configs[1].thinking_type.as_deref(), Some("enabled"));
        assert_eq!(configs[1].thinking_budget_tokens, Some(32768));
        assert_eq!(configs[1].max_tokens, Some(2048));
        assert_eq!(
            configs[1].custom_headers.get("x-test").map(String::as_str),
            Some("yes")
        );
        let mixin = load_mixin_config(d.path()).unwrap();
        assert_eq!(mixin.llm_nos, vec!["0", "backup"]);
        assert_eq!(mixin.max_retries, 5);
        assert_eq!(mixin.spring_back_secs, 7);
    }

    #[test]
    fn legacy_mykey_json_imports_default_models_and_mixin() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("mykey.json"),
            r#"{
  "mixin_config": {
    "llm_nos": ["gpt-native", 1],
    "max_retries": 9,
    "base_delay": 0.75,
    "spring_back": 42
  },
  "native_oai_config": {
    "name": "gpt-native",
    "apikey": "sk-primary",
    "apibase": "https://api.openai.com/v1",
    "model": "gpt-5.4",
    "api_mode": "responses",
    "stream": true,
    "read_timeout": 120,
    "connect_timeout": 8,
    "verify": false,
    "reasoning_effort": "high"
  },
  "native_claude_config0": {
    "name": "claude-native",
    "apikey": "sk-ant-secondary",
    "apibase": "https://api.anthropic.com",
    "model": "claude-opus-4-7",
    "user_agent": "claude-cli/2.1.113 (external, cli)",
    "headers": {"x-extra": "yes"},
    "thinking_type": "enabled",
    "thinking_budget_tokens": 32768,
    "read_timeout": 180
  }
}"#,
        )
        .unwrap();

        let legacy = load_legacy_mykey_config(d.path());
        let default = legacy.default.as_ref().unwrap();
        assert_eq!(default.name.as_deref(), Some("gpt-native"));
        assert_eq!(default.api_key.as_deref(), Some("sk-primary"));
        assert_eq!(
            default.base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(default.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(default.api_style.as_deref(), Some("responses"));
        assert_eq!(default.timeout_secs, Some(120));
        assert_eq!(default.connect_timeout_secs, Some(8));
        assert_eq!(default.verify_tls, Some(false));
        assert_eq!(default.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(legacy.models.len(), 2);
        assert!(
            legacy
                .models
                .iter()
                .any(|entry| entry.api_style.as_deref() == Some("claude"))
        );
        let claude = legacy
            .models
            .iter()
            .find(|entry| entry.api_style.as_deref() == Some("claude"))
            .unwrap();
        assert_eq!(claude.thinking_type.as_deref(), Some("enabled"));
        assert_eq!(claude.thinking_budget_tokens, Some(32768));
        let headers = claude.custom_headers.as_ref().unwrap();
        assert_eq!(
            headers.get("user-agent").map(String::as_str),
            Some("claude-cli/2.1.113 (external, cli)")
        );
        assert_eq!(headers.get("x-app").map(String::as_str), Some("cli"));
        assert_eq!(headers.get("x-extra").map(String::as_str), Some("yes"));
        assert_eq!(
            headers
                .get("anthropic-dangerous-direct-browser-access")
                .map(String::as_str),
            Some("true")
        );
        let mixin = legacy.mixin.unwrap();
        assert_eq!(mixin.llm_nos, vec!["gpt-native", "1"]);
        assert_eq!(mixin.max_retries, 9);
        assert_eq!(mixin.base_delay_secs, 0.75);
        assert_eq!(mixin.spring_back_secs, 42);
    }

    #[test]
    fn legacy_mykey_py_imports_simple_dict_assignments() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("mykey.py"),
            r#"
mixin_config = {
    'llm_nos': ['gpt-native'],
    'max_retries': 10,
    'base_delay': 0.5,
}

native_oai_config = {
    'name': 'gpt-native',
    'apikey': 'sk-primary',
    'apibase': 'https://api.openai.com/v1',
    'model': 'gpt-5.4',
    'api_mode': 'chat_completions',
    'stream': True,
    'read_timeout': 120,
    'connect_timeout': 6,
    'verify': False,
    'thinking_type': 'adaptive',
    'thinking_budget_tokens': 10240,
}
"#,
        )
        .unwrap();

        let legacy = load_legacy_mykey_config(d.path());
        let default = legacy.default.as_ref().unwrap();
        assert_eq!(default.name.as_deref(), Some("gpt-native"));
        assert_eq!(default.api_style.as_deref(), Some("chat"));
        assert_eq!(default.stream, Some(true));
        assert_eq!(default.timeout_secs, Some(120));
        assert_eq!(default.connect_timeout_secs, Some(6));
        assert_eq!(default.verify_tls, Some(false));
        assert_eq!(default.thinking_type.as_deref(), Some("adaptive"));
        assert_eq!(default.thinking_budget_tokens, Some(10240));
        let mixin = legacy.mixin.unwrap();
        assert_eq!(mixin.llm_nos, vec!["gpt-native"]);
        assert_eq!(mixin.max_retries, 10);
        assert_eq!(mixin.base_delay_secs, 0.5);
    }
}
