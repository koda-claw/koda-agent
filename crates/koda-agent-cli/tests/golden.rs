use koda_agent_core::{AgentConfig, AgentEvent, AgentResponse, AgentRuntime, ToolCall};
use koda_agent_llm::MockLlmClient;
use koda_agent_tools::GenericToolDispatcher;
use serde_json::{Value, json};
use std::{fs, process::Command, sync::Arc};
use tempfile::tempdir;

fn fixture_root() -> tempfile::TempDir {
    let d = tempdir().unwrap();
    fs::create_dir_all(d.path().join("assets")).unwrap();
    fs::create_dir_all(d.path().join("temp")).unwrap();
    fs::write(d.path().join("assets/tools_schema.json"), "[]").unwrap();
    fs::write(
        d.path().join("assets/sys_prompt.txt"),
        "You are GenericAgent.",
    )
    .unwrap();
    fs::write(d.path().join("temp/input.txt"), "alpha\n").unwrap();
    d
}

fn cfg(root: &std::path::Path) -> AgentConfig {
    AgentConfig {
        root_dir: root.into(),
        temp_dir: root.join("temp"),
        memory_dir: root.join("memory"),
        logs_dir: root.join("logs"),
        openai_base_url: "http://localhost".into(),
        openai_api_key: "sk-test".into(),
        openai_model: "mock".into(),
        llm_api_style: "chat".into(),
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

#[test]
fn doctor_json_reports_no_python_when_discovery_disabled() {
    let d = fixture_root();
    fs::write(
        d.path().join(".env"),
        "OPENAI_BASE_URL=http://localhost\nOPENAI_API_KEY=sk-test\nOPENAI_MODEL=mock\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_koda-agent"))
        .current_dir(d.path())
        .arg("doctor")
        .arg("--json")
        .env("KODA_DISABLE_PYTHON_DISCOVERY", "1")
        .env_remove("KODA_PYTHON")
        .env_remove("PYTHON")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["python"]["available"], false);
    assert_eq!(
        value["core"]["env_keys"]["OPENAI_API_KEY"], true,
        "doctor must report env presence without printing secrets"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("sk-test"));
}

#[tokio::test]
async fn golden_tool_then_final_answer() {
    let d = fixture_root();
    let cfg = cfg(d.path());
    let llm = Arc::new(MockLlmClient {
        responses: Arc::new(vec![
            AgentResponse {
                thinking: String::new(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: Some("call_1".into()),
                    name: "file_read".into(),
                    args: json!({"path":"input.txt"}),
                }],
                raw: Value::Null,
            },
            AgentResponse {
                thinking: String::new(),
                content: "saw alpha".into(),
                tool_calls: vec![],
                raw: Value::Null,
            },
        ]),
    });
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    let rt = AgentRuntime::new(cfg, llm, tools).unwrap();
    let out = rt.put_task("read the file").await.unwrap();
    assert!(out.contains("Tool: `file_read`"));
    assert!(out.contains("1|alpha"));
    assert!(out.contains("saw alpha"));
}

#[tokio::test]
async fn langfuse_trace_file_records_llm_and_tool_observations() {
    let d = fixture_root();
    fs::create_dir_all(d.path().join("config")).unwrap();
    fs::write(d.path().join("config/langfuse.toml"), "enabled = true\n").unwrap();
    let cfg = cfg(d.path());
    let llm = Arc::new(MockLlmClient {
        responses: Arc::new(vec![
            AgentResponse {
                thinking: String::new(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: Some("call_1".into()),
                    name: "file_read".into(),
                    args: json!({"path":"input.txt"}),
                }],
                raw: Value::Null,
            },
            AgentResponse {
                thinking: String::new(),
                content: "done".into(),
                tool_calls: vec![],
                raw: Value::Null,
            },
        ]),
    });
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    let rt = AgentRuntime::new(cfg.clone(), llm, tools).unwrap();
    rt.put_task("read input").await.unwrap();

    let trace = fs::read_to_string(cfg.logs_dir.join("langfuse_trace.jsonl")).unwrap();
    assert!(trace.contains(r#""name":"llm.chat""#));
    assert!(trace.contains(r#""type":"generation""#));
    assert!(trace.contains(r#""name":"file_read""#));
    assert!(trace.contains(r#""type":"tool""#));
}

#[tokio::test]
async fn slash_new_clears_history() {
    let d = fixture_root();
    let cfg = cfg(d.path());
    let llm = Arc::new(MockLlmClient {
        responses: Arc::new(vec![AgentResponse {
            thinking: String::new(),
            content: "ok".into(),
            tool_calls: vec![],
            raw: Value::Null,
        }]),
    });
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    let rt = AgentRuntime::new(cfg, llm, tools).unwrap();
    rt.put_task("hello").await.unwrap();
    assert!(!rt.history_info().is_empty());
    let out = rt.put_task("/new").await.unwrap();
    assert!(out.contains("新会话"));
    assert!(rt.history_info().is_empty());
}

#[tokio::test]
async fn slash_session_sets_extra_system_prompt() {
    let d = fixture_root();
    let cfg = cfg(d.path());
    let llm = Arc::new(MockLlmClient {
        responses: Arc::new(vec![AgentResponse {
            thinking: String::new(),
            content: "ok".into(),
            tool_calls: vec![],
            raw: Value::Null,
        }]),
    });
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    let rt = AgentRuntime::new(cfg.clone(), llm, tools).unwrap();
    let out = rt
        .put_task(r#"/session.extra_sys_prompt=" EXTRA""#)
        .await
        .unwrap();
    assert!(out.contains("session.extra_sys_prompt"));
    rt.put_task("hello").await.unwrap();
    let log = fs::read_to_string(
        cfg.temp_dir
            .join("model_responses")
            .join(format!("model_responses_{}.txt", std::process::id())),
    )
    .unwrap();
    assert!(log.contains("EXTRA"));
}

#[tokio::test]
async fn final_output_formats_summary_and_file_content() {
    let d = fixture_root();
    let cfg = cfg(d.path());
    let llm = Arc::new(MockLlmClient {
        responses: Arc::new(vec![AgentResponse {
            thinking: String::new(),
            content: "<summary>完成</summary><file_content>\nabc\n</file_content>".into(),
            tool_calls: vec![],
            raw: Value::Null,
        }]),
    });
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    let rt = AgentRuntime::new(cfg, llm, tools).unwrap();
    let out = rt.put_task("format").await.unwrap();
    assert!(!out.contains("<summary>"));
    assert!(out.contains("💭 完成"));
    assert!(out.contains("````\n<file_content>"));
}

#[tokio::test]
async fn runtime_emits_ordered_events() {
    let d = fixture_root();
    let cfg = cfg(d.path());
    let llm = Arc::new(MockLlmClient {
        responses: Arc::new(vec![
            AgentResponse {
                thinking: String::new(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: Some("call_1".into()),
                    name: "file_read".into(),
                    args: json!({"path":"input.txt"}),
                }],
                raw: Value::Null,
            },
            AgentResponse {
                thinking: String::new(),
                content: "done".into(),
                tool_calls: vec![],
                raw: Value::Null,
            },
        ]),
    });
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    let rt = AgentRuntime::new(cfg, llm, tools).unwrap();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = std::sync::Arc::clone(&events);
    rt.put_task_with_events("read the file", move |event| {
        captured.lock().unwrap().push(event);
    })
    .await
    .unwrap();
    let events = events.lock().unwrap();
    assert!(matches!(events[0], AgentEvent::TurnStarted { turn: 1 }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStarted { name, .. } if name == "file_read"))
    );
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::AssistantMessage { content, .. } if content == "done")
        )
    );
}

#[tokio::test]
async fn no_tool_large_code_block_requests_followup() {
    let d = fixture_root();
    let mut cfg = cfg(d.path());
    cfg.max_turns = 2;
    let llm = Arc::new(MockLlmClient {
        responses: Arc::new(vec![
            AgentResponse {
                thinking: String::new(),
                content: "<summary>只输出代码</summary>\n```python\nprint('hello')\nprint('hello')\nprint('hello')\nprint('hello')\nprint('hello')\n```".into(),
                tool_calls: vec![],
                raw: Value::Null,
            },
            AgentResponse {
                thinking: String::new(),
                content: "<summary>补充说明</summary>\n这是展示代码，不需要执行。".into(),
                tool_calls: vec![],
                raw: Value::Null,
            },
        ]),
    });
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    let rt = AgentRuntime::new(cfg, llm, tools).unwrap();
    let out = rt.put_task("show code").await.unwrap();
    assert!(out.contains("No-tool response requires another turn"));
    assert!(out.contains("这是展示代码"));
}

#[tokio::test]
async fn plan_mode_intercepts_unverified_completion_claim() {
    let d = fixture_root();
    fs::create_dir_all(d.path().join("temp/plan_demo")).unwrap();
    fs::write(d.path().join("temp/_plan_mode"), "./plan_demo/plan.md").unwrap();
    fs::write(d.path().join("temp/plan_demo/plan.md"), "- [ ] verify\n").unwrap();
    let mut cfg = cfg(d.path());
    cfg.max_turns = 2;
    let llm = Arc::new(MockLlmClient {
        responses: Arc::new(vec![
            AgentResponse {
                thinking: String::new(),
                content: "任务完成".into(),
                tool_calls: vec![],
                raw: Value::Null,
            },
            AgentResponse {
                thinking: String::new(),
                content: "VERDICT: PASS".into(),
                tool_calls: vec![],
                raw: Value::Null,
            },
        ]),
    });
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    let rt = AgentRuntime::new(cfg, llm, tools).unwrap();
    let out = rt.put_task("run plan").await.unwrap();
    assert!(out.contains("Plan completion claim intercepted"));
    assert!(out.contains("VERDICT: PASS"));
}

#[tokio::test]
async fn inline_done_hook_keeps_loop_alive_after_empty_tool_prompt() {
    let d = fixture_root();
    let mut cfg = cfg(d.path());
    cfg.max_turns = 2;
    let llm = Arc::new(MockLlmClient {
        responses: Arc::new(vec![
            AgentResponse {
                thinking: String::new(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: Some("call_hook".into()),
                    name: "code_run".into(),
                    args: json!({"inline_eval":true,"script":"handler._done_hooks.append('收尾检查')"}),
                }],
                raw: Value::Null,
            },
            AgentResponse {
                thinking: String::new(),
                content: "hook consumed".into(),
                tool_calls: vec![],
                raw: Value::Null,
            },
        ]),
    });
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    let rt = AgentRuntime::new(cfg, llm, tools).unwrap();
    let out = rt.put_task("register hook").await.unwrap();
    assert!(out.contains("Registered done hook"));
    assert!(out.contains("hook consumed"));
}

#[tokio::test]
async fn stop_file_exits_at_turn_end() {
    let d = fixture_root();
    let mut cfg = cfg(d.path());
    cfg.max_turns = 2;
    let llm = Arc::new(MockLlmClient {
        responses: Arc::new(vec![
            AgentResponse {
                thinking: String::new(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: Some("call_stop".into()),
                    name: "file_write".into(),
                    args: json!({"path":"_stop","content":"1"}),
                }],
                raw: Value::Null,
            },
            AgentResponse {
                thinking: String::new(),
                content: "should not run".into(),
                tool_calls: vec![],
                raw: Value::Null,
            },
        ]),
    });
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    let rt = AgentRuntime::new(cfg, llm, tools).unwrap();
    let out = rt.put_task("stop after turn").await.unwrap();
    assert!(out.contains("_stop requested"));
    assert!(!out.contains("should not run"));
}

#[tokio::test]
async fn long_tasks_get_memory_settlement_prompt_before_final() {
    let d = fixture_root();
    let mut cfg = cfg(d.path());
    cfg.max_turns = 16;
    let mut responses = Vec::new();
    for i in 0..14 {
        responses.push(AgentResponse {
            thinking: String::new(),
            content: format!("<summary>第{i}轮读文件</summary>"),
            tool_calls: vec![ToolCall {
                id: Some(format!("call_{i}")),
                name: "file_read".into(),
                args: json!({"path":"input.txt"}),
            }],
            raw: Value::Null,
        });
    }
    responses.push(AgentResponse {
        thinking: String::new(),
        content: "<summary>准备结束</summary>final".into(),
        tool_calls: vec![],
        raw: Value::Null,
    });
    responses.push(AgentResponse {
        thinking: String::new(),
        content: "<summary>结算后结束</summary>done".into(),
        tool_calls: vec![],
        raw: Value::Null,
    });
    let llm = Arc::new(MockLlmClient {
        responses: Arc::new(responses),
    });
    let tools = Arc::new(GenericToolDispatcher::new(cfg.clone()));
    let rt = AgentRuntime::new(cfg, llm, tools).unwrap();
    let out = rt.put_task("long work").await.unwrap();
    assert!(out.contains("Long task settlement required"));
    assert!(out.contains("结算后结束"));
}
