use super::{
    discovery::{responses_mcp_tool, ActiveTools},
    dispatch::RoundScope,
    events::{AgentEvent, EventLog},
    mcp_runtime::McpRuntime,
    response::extract_output_text,
    Agent, ModelTarget, RunOrigin, RunRequest, SubagentModels,
};
use crate::{
    application::{
        approval::{AlwaysApprove, DenyApproval},
        input::InputPart,
        ports::{ApprovalDecision, ApprovalHandler, ApprovalSource, McpApprovalRequest},
        registry::ToolRegistry,
        settings::AgentSettings,
    },
    domain::{
        environment::CheckConfig,
        mcp::{McpApprovalMode, McpServerConfig, McpToolCatalog, McpTransport},
        plan::{RunOutcome, TaskGoal, TaskPlan},
        policy::UserPolicy,
        session::SessionBinding,
        tool::{ToolContext, ToolDefinition},
        usage::{StopReason, UsageSummary},
    },
    infrastructure::{
        mcp::McpPool, openai::OpenAiClient, session_store::Session, tools::register_builtin_tools,
    },
};
use anyhow::Result;
use async_trait::async_trait;
use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

async fn with_scope<T>(active: &ActiveTools, body: impl AsyncFnOnce(RoundScope<'_>) -> T) -> T {
    let context = ToolContext::default();
    let runtime = McpRuntime::default();
    let events = EventLog::new(None);
    let plan = Mutex::new(TaskPlan::default());
    let delegated_usage = Mutex::new(UsageSummary::default());
    body(RoundScope {
        conversation: None,
        call_id: None,
        round: 0,
        user_request: "",
        tool_context: &context,
        active,
        mcp_runtime: &runtime,
        events: &events,
        plan: &plan,
        depth: 0,
        read_only: false,
        token_budget: None,
        delegated_usage: &delegated_usage,
    })
    .await
}

fn registry_with(tools: &[(&str, &str)]) -> ToolRegistry {
    let registry = ToolRegistry::new();
    for (name, description) in tools {
        registry
            .register(
                ToolDefinition::new(
                    *name,
                    *description,
                    json!({"type": "object", "properties": {}}),
                ),
                |_arguments| async move { Ok(json!({"ok": true})) },
            )
            .unwrap();
    }
    registry
}

fn agent(registry: ToolRegistry, mcp_servers: Vec<McpServerConfig>) -> Agent {
    Agent::new(
        OpenAiClient::new("test", "http://127.0.0.1:1234/v1"),
        AgentSettings::default(),
        Arc::new(McpPool::new(mcp_servers)),
        registry,
        UserPolicy::default(),
        Arc::new(AlwaysApprove),
    )
}

fn selected_local_tools(names: &[&str]) -> ActiveTools {
    ActiveTools {
        local: names.iter().map(|name| (*name).to_string()).collect(),
        ..ActiveTools::default()
    }
}

#[derive(Clone)]
struct MockResponsesState {
    responses: Arc<Mutex<VecDeque<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

struct MockResponses {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockResponses {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mock_response(
    State(state): State<MockResponsesState>,
    Json(payload): Json<Value>,
) -> Json<Value> {
    state.requests.lock().unwrap().push(payload);
    Json(
        state
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| json!({"error": {"message": "unexpected extra request"}})),
    )
}

async fn mock_responses(responses: Vec<Value>) -> MockResponses {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = MockResponsesState {
        responses: Arc::new(Mutex::new(responses.into())),
        requests: Arc::clone(&requests),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/responses", post(mock_response))
        .with_state(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    MockResponses {
        url,
        requests,
        task,
    }
}

fn text_response(id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "status": "completed",
        "output": [{
            "type": "message",
            "content": [{"type": "output_text", "text": text}]
        }]
    })
}

fn search_response(id: &str, query: &str) -> Value {
    json!({
        "id": id,
        "status": "completed",
        "output": [{
            "type": "function_call",
            "call_id": format!("{id}_search"),
            "name": "tool_search",
            "arguments": json!({"query": query}).to_string()
        }]
    })
}

fn counting_registry() -> (ToolRegistry, Arc<AtomicUsize>) {
    let registry = ToolRegistry::new();
    let count = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&count);
    registry
        .register(
            ToolDefinition::new(
                "record_action",
                "Record an action",
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
            ),
            move |_arguments| {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"saved": true}))
                }
            },
        )
        .unwrap();
    (registry, count)
}

fn request() -> RunRequest {
    RunRequest::new(vec![InputPart::Text("Do the requested work".into())])
}

#[test]
fn extracts_text_from_raw_response_items() {
    let response = json!({
        "output": [{
            "type": "message",
            "content": [{"type": "output_text", "text": "done"}]
        }]
    });
    assert_eq!(extract_output_text(&response), "done");
}

#[test]
fn initial_tool_payload_is_constant_size() {
    let agent = Agent::new(
        OpenAiClient::new("test", "http://127.0.0.1:1234/v1"),
        AgentSettings::default(),
        Arc::new(McpPool::new(Vec::new())),
        registry_with(&[("echo", "Return a value")]),
        UserPolicy::default(),
        Arc::new(DenyApproval),
    );

    let tools = agent
        .response_tools(&ActiveTools::default(), &McpRuntime::default(), 0, false)
        .unwrap();
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["tool_search", "task_plan", "delegate_task"]);

    // Sub-agents cannot delegate further.
    let tools = agent
        .response_tools(&ActiveTools::default(), &McpRuntime::default(), 1, false)
        .unwrap();
    assert_eq!(tools.len(), 2);
}

#[test]
fn search_loads_only_matching_tools() {
    let agent = agent(
        registry_with(&[
            ("read_file", "Read a file"),
            ("send_email", "Send an email"),
        ]),
        Vec::new(),
    );

    let selection = agent
        .search_tools(
            &json!({"query": "read file"}),
            &McpRuntime::default(),
            &ToolContext::default(),
        )
        .unwrap();
    assert!(selection.active.local.contains("read_file"));
    assert!(!selection.active.local.contains("send_email"));
    assert_eq!(selection.results.len(), 1);
}

#[test]
fn search_returns_up_to_the_discovery_limit() {
    let agent = agent(
        registry_with(&[
            ("read_file", "Read a file"),
            ("read_dir", "Read a directory"),
            ("read_url", "Read a URL"),
        ]),
        Vec::new(),
    );

    let selection = agent
        .search_tools(
            &json!({"query": "read"}),
            &McpRuntime::default(),
            &ToolContext::default(),
        )
        .unwrap();
    assert_eq!(selection.results.len(), 3);
}

#[test]
fn server_description_alone_does_not_select_every_tool_on_the_server() {
    let server = McpServerConfig {
        description: Some("アニメ情報を確認できます".into()),
        tool_catalog: Some(vec![
            McpToolCatalog {
                name: "search_works".into(),
                description: Some("Search anime works".into()),
            },
            McpToolCatalog {
                name: "update_status".into(),
                description: Some("Update the watch status".into()),
            },
        ]),
        ..remote_server("annict")
    };
    let agent = agent(
        registry_with(&[("unix_time", "Return the current time")]),
        vec![server],
    );
    let search = |query: &str| {
        let selection = agent
            .search_tools(
                &json!({ "query": query }),
                &McpRuntime::default(),
                &ToolContext::default(),
            )
            .unwrap();
        selection
            .results
            .iter()
            .map(|result| result["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };

    assert_eq!(search("時刻 確認"), Vec::<String>::new());
    assert_eq!(search("time 確認"), vec!["unix_time"]);
    assert_eq!(search("works 確認"), vec!["search_works"]);
    assert_eq!(search("annict"), vec!["search_works", "update_status"]);
}

#[test]
fn the_tool_catalog_names_every_tool_that_a_search_can_load() {
    let registry = registry_with(&[
        ("workspace_read", "Read a file"),
        ("send_email", "Send an email"),
        ("workspace_exec", "Run a command"),
    ]);
    registry
        .register(
            ToolDefinition::new("skill_read", "Read a skill", json!({"type": "object"}))
                .always_offered(),
            |_arguments| async move { Ok(json!({})) },
        )
        .unwrap();
    let server = McpServerConfig {
        description: Some("アニメ情報を\n確認できます".into()),
        tool_catalog: Some(vec![
            McpToolCatalog {
                name: "search_works".into(),
                description: None,
            },
            McpToolCatalog {
                name: "update_status".into(),
                description: None,
            },
        ]),
        ..remote_server("annict")
    };
    let mut agent = agent(registry, vec![server, remote_server("empty")]);
    agent.policy = UserPolicy::new(vec!["send_email".into()], None);

    let catalog = agent
        .tool_catalog(&McpRuntime::default(), &ToolContext::default())
        .unwrap();
    assert!(catalog.contains("\n- Local tools: workspace_read\n"));
    assert!(catalog.ends_with(
        "\n- MCP server annict (アニメ情報を 確認できます): search_works, update_status"
    ));
    // Disabled, unrunnable, and always offered tools, and servers with
    // nothing to load, are left out.
    for name in ["send_email", "workspace_exec", "skill_read", "empty"] {
        assert!(!catalog.contains(name), "{name} should not be listed");
    }

    let context = ToolContext {
        allow_exec: true,
        ..ToolContext::default()
    };
    let catalog = agent
        .tool_catalog(&McpRuntime::default(), &context)
        .unwrap();
    assert!(catalog.contains("- Local tools: workspace_exec, workspace_read\n"));

    agent.policy = UserPolicy::new(vec!["tool_search".into()], None);
    assert!(agent
        .tool_catalog(&McpRuntime::default(), &ToolContext::default())
        .is_none());
}

#[test]
fn the_tool_catalog_is_cut_at_its_size_limit() {
    let names = (0..2000)
        .map(|index| (format!("tool_{index:04}"), "A tool"))
        .collect::<Vec<_>>();
    let names = names
        .iter()
        .map(|(name, description)| (name.as_str(), *description))
        .collect::<Vec<_>>();
    let agent = agent(registry_with(&names), Vec::new());

    let catalog = agent
        .tool_catalog(&McpRuntime::default(), &ToolContext::default())
        .unwrap();
    assert!(catalog.len() < 9 * 1024);
    assert!(catalog.contains("tool_0000, tool_0001"));
    assert!(!catalog.contains("tool_1999"));
    assert!(catalog.ends_with("tool_search finds them too.)"));
}

#[tokio::test]
async fn runs_list_the_loadable_tools_in_the_instructions() {
    let server = mock_responses(vec![text_response("done", "Done")]).await;
    let mut agent = agent(
        registry_with(&[("unix_time", "Return the time")]),
        Vec::new(),
    );
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));

    agent.run(request()).await.unwrap();

    let requests = server.requests.lock().unwrap();
    let instructions = requests[0]["instructions"].as_str().unwrap();
    assert!(instructions.contains("# Tools you can load"));
    assert!(instructions.contains("- Local tools: unix_time"));
    let tools = requests[0]["tools"].as_array().unwrap();
    assert!(tools.iter().all(|tool| tool["name"] != "unix_time"));
}

#[tokio::test]
async fn invalid_arguments_are_returned_to_the_model() {
    let agent = agent(registry_with(&[("echo_tool", "Echo")]), Vec::new());
    let (output, _) = with_scope(&ActiveTools::default(), async |scope| {
        agent
            .handle_function_call("echo_tool", &json!("{not json"), scope)
            .await
    })
    .await
    .unwrap();
    assert_eq!(output["error"], "invalid_arguments");
}

/// Two tools that each wait for the other: they only finish when run at
/// the same time.
fn barrier_registry() -> ToolRegistry {
    let registry = ToolRegistry::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    for name in ["first", "second"] {
        let barrier = Arc::clone(&barrier);
        registry
            .register(
                ToolDefinition::new(name, name, json!({"type": "object", "properties": {}})),
                move |_arguments| {
                    let barrier = Arc::clone(&barrier);
                    async move {
                        barrier.wait().await;
                        Ok(json!({"tool": name}))
                    }
                },
            )
            .unwrap();
    }
    registry
}

fn calls(names: &[&str]) -> Vec<Value> {
    names
        .iter()
        .map(|name| {
            json!({"type": "function_call", "call_id": format!("call_{name}"), "name": name, "arguments": "{}"})
        })
        .collect()
}

#[tokio::test]
async fn runs_tool_calls_from_one_response_concurrently() {
    let agent = agent(barrier_registry(), Vec::new());
    let items = calls(&["first", "second"]);
    let active = selected_local_tools(&["first", "second"]);

    let (continuation, _) = with_scope(&active, async |scope| {
        tokio::time::timeout(
            Duration::from_secs(5),
            agent.handle_output_items(&items, scope, None),
        )
        .await
    })
    .await
    .expect("tool calls did not run concurrently")
    .unwrap();

    let call_ids = continuation
        .iter()
        .map(|item| item["call_id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(call_ids, ["call_first", "call_second"]);
}

#[tokio::test]
async fn runs_tool_calls_sequentially_when_parallel_calls_are_disabled() {
    let mut agent = agent(barrier_registry(), Vec::new());
    agent.settings.parallel_tool_calls = false;
    let items = calls(&["first", "second"]);
    let active = selected_local_tools(&["first", "second"]);

    let result = with_scope(&active, async |scope| {
        tokio::time::timeout(
            Duration::from_millis(300),
            agent.handle_output_items(&items, scope, None),
        )
        .await
    })
    .await;
    assert!(result.is_err(), "calls should not overlap");
}

#[tokio::test]
async fn tool_search_selection_applies_after_the_round() {
    let agent = agent(
        registry_with(&[
            ("read_file", "Read a file"),
            ("send_email", "Send an email"),
        ]),
        Vec::new(),
    );
    let mut items = vec![json!({
        "type": "function_call", "call_id": "search", "name": "tool_search",
        "arguments": "{\"query\": \"read\"}"
    })];
    items.extend(calls(&["send_email"]));

    let (continuation, selection) = with_scope(&ActiveTools::default(), async |scope| {
        agent.handle_output_items(&items, scope, None).await
    })
    .await
    .unwrap();
    assert_eq!(continuation.len(), 2);
    let blocked: Value = serde_json::from_str(continuation[1]["output"].as_str().unwrap()).unwrap();
    assert_eq!(blocked["error"], "tool_not_selected");
    assert!(selection.unwrap().local.contains("read_file"));
}

#[tokio::test]
async fn rejects_unselected_calls_and_executes_selected_calls_over_http() {
    let server = mock_responses(vec![
        json!({"id": "unselected", "status": "completed", "output": calls(&["record_action"])}),
        search_response("search", "record_action"),
        json!({"id": "selected", "status": "completed", "output": calls(&["record_action"])}),
        text_response("done", "Action recorded"),
    ])
    .await;
    let (registry, count) = counting_registry();
    let mut agent = agent(registry, Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    agent.settings.max_tool_rounds = 4;

    let result = agent.run(request()).await.unwrap();

    assert_eq!(result.text, "Action recorded");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let requests = server.requests.lock().unwrap();
    let blocked: Value =
        serde_json::from_str(requests[1]["input"][0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(blocked["error"], "tool_not_selected");
    assert_eq!(requests[1]["previous_response_id"], "unselected");
    assert!(requests[2]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "record_action"));
    assert_eq!(requests[3]["tools"], json!([]));
    assert_eq!(requests[3]["tool_choice"], "none");
    let saved: Value =
        serde_json::from_str(requests[3]["input"][0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(saved["saved"], true);
}

#[tokio::test]
async fn progress_messages_do_not_end_the_task() {
    let server = mock_responses(vec![
        json!({"id":"progress","status":"completed","output":[{"type":"message","phase":"commentary","role":"assistant","content":[{"type":"output_text","text":"I will inspect the files."}]}]}),
        text_response("done", "Inspection complete"),
    ]).await;
    let mut agent = agent(ToolRegistry::new(), Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    agent.settings.max_tool_rounds = 2;
    let result = agent.run(request()).await.unwrap();
    assert_eq!(result.text, "Inspection complete");
    assert!(
        matches!(&result.events[0], AgentEvent::AssistantProgress { text, .. } if text == "I will inspect the files.")
    );
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn mixed_progress_and_final_messages_return_only_the_final_answer() {
    let server = mock_responses(vec![json!({"id":"mixed","status":"completed","output":[
        {"type":"message","phase":"commentary","content":[{"type":"output_text","text":"Checking."}]},
        {"type":"message","phase":"final_answer","content":[{"type":"output_text","text":"Done."}]}
    ]})]).await;
    let mut agent = agent(ToolRegistry::new(), vec![]);
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    let result = agent.run(request()).await.unwrap();
    assert_eq!(result.text, "Done.");
    assert!(
        matches!(&result.events[0], AgentEvent::AssistantProgress { text, .. } if text == "Checking.")
    );
}

#[tokio::test]
async fn aggregate_output_text_is_preserved_alongside_non_message_items() {
    let server = mock_responses(vec![
        json!({"id":"aggregate","status":"completed","output_text":"Done.","output":[
            {"type":"reasoning","summary":[]}
        ]}),
    ])
    .await;
    let mut agent = agent(ToolRegistry::new(), vec![]);
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    assert_eq!(agent.run(request()).await.unwrap().text, "Done.");
}

#[tokio::test]
async fn disabled_task_plan_cannot_be_called_or_exposed() {
    let mut agent = agent(ToolRegistry::new(), vec![]);
    agent.policy = UserPolicy::new(vec!["task_plan".into()], None);
    assert!(!agent
        .response_tools(&ActiveTools::default(), &McpRuntime::default(), 0, false)
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "task_plan"));
    let (output, _) = with_scope(&ActiveTools::default(), async |scope| {
        let result = agent
            .handle_function_call("task_plan", &json!({"steps":null}), scope)
            .await
            .unwrap();
        assert_eq!(scope.plan.lock().unwrap().revision, 0);
        result
    })
    .await;
    assert_eq!(output["error"], "tool_disabled");
}

#[tokio::test]
async fn interrupted_parallel_run_preserves_completed_calls_only() {
    let (registry, count) = counting_registry();
    registry
        .register(
            ToolDefinition::new("slow_action", "Wait forever", json!({"type":"object"})),
            |_| async { std::future::pending::<Result<Value>>().await },
        )
        .unwrap();
    let server = mock_responses(vec![
        search_response("discover", ""),
        json!({"id":"actions","status":"completed","output":[
            {"type":"function_call","call_id":"slow","name":"slow_action","arguments":"{}"},
            {"type":"function_call","call_id":"fast","name":"record_action","arguments":"{}"}
        ]}),
    ])
    .await;
    let mut agent = agent(registry, vec![]);
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("interrupted.json");
    let binding = SessionBinding::new(&ToolContext::default(), &server.url).unwrap();
    let mut session = Session::open(&path, binding.clone(), false).unwrap();
    {
        let mut run = Box::pin(agent.run_in_session(request(), &mut session));
        let observe_checkpoint = async {
            loop {
                if Session::inspect(&path).is_ok_and(|data| {
                    data.history.iter().any(|item| {
                        item["type"] == "function_call_output" && item["call_id"] == "fast"
                    })
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            result = &mut run => panic!("slow call unexpectedly finished: {result:?}"),
            observed = tokio::time::timeout(Duration::from_secs(5), observe_checkpoint) => observed.expect("fast result was not checkpointed while slow call remained pending"),
        }
        // Dropping the future simulates interruption before the round ends.
    }
    drop(session);
    assert!(Session::open(&path, binding.clone(), false).is_err());
    let recovered = Session::open(&path, binding, true).unwrap();
    let outputs = recovered
        .data()
        .history
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect::<Vec<_>>();
    let fast = outputs
        .iter()
        .filter(|item| item["call_id"] == "fast")
        .collect::<Vec<_>>();
    assert_eq!(fast.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(fast[0]["output"].as_str().unwrap()).unwrap()["saved"],
        true
    );
    let slow = outputs
        .iter()
        .find(|item| item["call_id"] == "slow")
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(slow["output"].as_str().unwrap()).unwrap()["error"],
        "execution_interrupted"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn configured_check_deadline_preserves_output_past_generic_tool_timeout() {
    let registry = ToolRegistry::new();
    register_builtin_tools(&registry).unwrap();
    let server = mock_responses(vec![search_response("search", "workspace_check"), json!({"id":"check","status":"completed","output":[
        {"type":"function_call","call_id":"check1","name":"workspace_check","arguments":"{\"name\":\"slow\"}"}
    ]}), text_response("final", "Check timed out")]).await;
    let mut agent = agent(registry, vec![]);
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    agent.settings.tool_timeout_secs = 1;
    let directory = tempfile::tempdir().unwrap();
    let mut request = request();
    request.context.workspace = Some(directory.path().into());
    request.context.checks.insert(
        "slow".into(),
        CheckConfig {
            program: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec![
                "--exact".into(),
                "infrastructure::tools::checks::tests::slow_check_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            description: String::new(),
            timeout_secs: 2,
        },
    );
    let result = tokio::time::timeout(Duration::from_secs(10), agent.run(request))
        .await
        .unwrap()
        .unwrap();
    let checked = result
        .events
        .iter()
        .find_map(|event| match event {
            AgentEvent::LocalToolResult { name, output, .. } if name == "workspace_check" => {
                Some(output)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(checked["error"], "check_timed_out");
    assert_eq!(checked["timed_out"], true);
    assert!(checked["stdout"]
        .as_str()
        .unwrap()
        .contains("started slow check"));
}

#[tokio::test]
async fn a_new_search_revokes_previous_local_selection() {
    let server = mock_responses(vec![
        search_response("first", "record_action"),
        search_response("second", "no_matching_capability"),
        json!({"id": "stale", "status": "completed", "output": calls(&["record_action"])}),
        text_response("done", "No action executed"),
    ])
    .await;
    let (registry, count) = counting_registry();
    let mut agent = agent(registry, Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    agent.settings.max_tool_rounds = 4;

    agent.run(request()).await.unwrap();

    assert_eq!(count.load(Ordering::SeqCst), 0);
    let requests = server.requests.lock().unwrap();
    let blocked: Value =
        serde_json::from_str(requests[3]["input"][0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(blocked["error"], "tool_not_selected");
}

#[tokio::test]
async fn unfinished_responses_never_execute_tools_or_succeed_with_partial_text() {
    for status in ["incomplete", "failed", "cancelled", "in_progress", "queued"] {
        for includes_tool in [false, true] {
            let mut response = text_response("unfinished", "This is only a partial answer");
            response["status"] = json!(status);
            response["incomplete_details"] = json!({"reason": "max_output_tokens"});
            if includes_tool {
                response["output"]
                    .as_array_mut()
                    .unwrap()
                    .extend(calls(&["record_action"]));
            }
            let server =
                mock_responses(vec![search_response("search", "record_action"), response]).await;
            let (registry, count) = counting_registry();
            let mut agent = agent(registry, Vec::new());
            agent.client = Arc::new(OpenAiClient::new("test", &server.url));

            let error = agent.run(request()).await.unwrap_err();

            assert!(error.to_string().contains(status), "{error}");
            assert_eq!(count.load(Ordering::SeqCst), 0);
            assert_eq!(server.requests.lock().unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn returns_refusal_text_to_the_caller() {
    let server = mock_responses(vec![json!({
        "id": "refusal", "status": "completed", "output": [{
            "type": "message",
            "content": [{"type": "refusal", "refusal": "I cannot help with that request."}]
        }]
    })])
    .await;
    let mut agent = agent(ToolRegistry::new(), Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));

    let result = agent.run(request()).await.unwrap();

    assert_eq!(result.text, "I cannot help with that request.");
    assert_eq!(result.response_id, "refusal");
}

#[tokio::test]
async fn one_round_budget_requests_a_final_answer_without_tools() {
    let server = mock_responses(vec![text_response("done", "Here is my answer")]).await;
    let mut agent = agent(ToolRegistry::new(), Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    agent.settings.max_tool_rounds = 1;

    assert_eq!(
        agent.run(request()).await.unwrap().text,
        "Here is my answer"
    );

    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["tools"], json!([]));
    assert_eq!(requests[0]["tool_choice"], "none");
}

#[tokio::test]
async fn never_executes_calls_requested_during_the_final_response() {
    let server = mock_responses(vec![
        search_response("search", "record_action"),
        json!({"id": "final", "status": "completed", "output": calls(&["record_action"])}),
    ])
    .await;
    let (registry, count) = counting_registry();
    let mut agent = agent(registry, Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    agent.settings.max_tool_rounds = 2;

    let error = agent.run(request()).await.unwrap_err();

    assert!(error.to_string().contains("final response"));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["tools"], json!([]));
    assert_eq!(requests[1]["tool_choice"], "none");
}

struct CountingApproval {
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

#[async_trait]
impl ApprovalHandler for CountingApproval {
    async fn approve(&self, _request: McpApprovalRequest) -> Result<bool> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(true)
    }
}

/// Records the request it receives and explains its decision.
struct ExplainingApproval {
    seen: Mutex<Vec<McpApprovalRequest>>,
}

#[async_trait]
impl ApprovalHandler for ExplainingApproval {
    async fn approve(&self, _request: McpApprovalRequest) -> Result<bool> {
        unreachable!("the agent asks for a decision with a reason")
    }

    async fn decide(&self, request: McpApprovalRequest) -> Result<ApprovalDecision> {
        self.seen.lock().unwrap().push(request);
        Ok(ApprovalDecision {
            approved: false,
            reason: Some("auto: not part of the request".into()),
        })
    }
}

#[tokio::test]
async fn approval_handlers_see_the_request_and_their_reasons_are_reported() {
    let server = McpServerConfig {
        label: "github".into(),
        transport: McpTransport::Responses,
        url: Some("https://example.test/mcp".into()),
        tunnel_id: None,
        command: None,
        args: vec![],
        cwd: None,
        env_vars: Default::default(),
        description: None,
        authorization_env: None,
        oauth: false,
        oauth_scopes: None,
        allowed_tools: None,
        disabled_tools: vec![],
        tool_catalog: Some(vec![McpToolCatalog {
            name: "close_issue".into(),
            description: Some("Close an issue".into()),
        }]),
        require_approval: McpApprovalMode::Always,
        reuse_connection: true,
    };
    let server_mock = mock_responses(vec![
        search_response("r1", "close issue"),
        json!({"id": "r2", "status": "completed", "output": [{
            "type": "mcp_approval_request", "id": "approval_1", "server_label": "github",
            "name": "close_issue", "arguments": "{\"number\":7}"
        }]}),
        text_response("r3", "Not closed."),
    ])
    .await;
    let handler = Arc::new(ExplainingApproval {
        seen: Mutex::new(Vec::new()),
    });
    let agent = Agent::new(
        OpenAiClient::new("test", &server_mock.url),
        AgentSettings::default(),
        Arc::new(McpPool::new(vec![server])),
        ToolRegistry::new(),
        UserPolicy::default(),
        handler.clone(),
    );
    let result = agent
        .run(RunRequest::new(vec![InputPart::Text(
            "Summarize issue 7".into(),
        )]))
        .await
        .unwrap();

    let seen = handler.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].user_request, "Summarize issue 7");
    assert_eq!(seen[0].tool_description.as_deref(), Some("Close an issue"));
    assert_eq!(seen[0].arguments, json!({"number": 7}));
    assert!(result.events.iter().any(|event| matches!(
        event,
        AgentEvent::McpApproval { approved: false, reason: Some(reason), .. }
            if reason == "auto: not part of the request"
    )));
    let requests = server_mock.requests.lock().unwrap();
    assert_eq!(requests[2]["input"][0]["approve"], false);
}

#[tokio::test]
async fn responses_mcp_approval_requires_policy_and_selection() {
    let server = McpServerConfig {
        label: "github".into(),
        transport: McpTransport::Responses,
        url: Some("https://example.test/mcp".into()),
        tunnel_id: None,
        command: None,
        args: vec![],
        cwd: None,
        env_vars: Default::default(),
        description: None,
        authorization_env: None,
        oauth: false,
        oauth_scopes: None,
        allowed_tools: None,
        disabled_tools: vec![],
        tool_catalog: Some(vec![McpToolCatalog {
            name: "list_issues".into(),
            description: None,
        }]),
        require_approval: McpApprovalMode::Always,
        reuse_connection: true,
    };
    let agent = agent(ToolRegistry::new(), vec![server.clone()]);
    let request = |tool: &str| McpApprovalRequest {
        approval_request_id: "req".into(),
        server_label: "github".into(),
        tool_name: tool.into(),
        arguments: json!({}),
        ..McpApprovalRequest::default()
    };

    let empty = ActiveTools::default();
    assert!(!with_scope(&empty, async |scope| agent
        .approve_responses_mcp_call(&request("list_issues"), scope)
        .await)
    .await
    .unwrap());

    let mut active = ActiveTools::default();
    active
        .responses_mcp
        .insert("github".into(), vec!["list_issues".into()]);
    assert!(with_scope(&active, async |scope| agent
        .approve_responses_mcp_call(&request("list_issues"), scope)
        .await)
    .await
    .unwrap());
    assert!(!with_scope(&active, async |scope| agent
        .approve_responses_mcp_call(&request("delete_issue"), scope)
        .await)
    .await
    .unwrap());

    // Concurrent approval requests reach the handler one at a time.
    let handler = Arc::new(CountingApproval {
        in_flight: AtomicUsize::new(0),
        max_in_flight: AtomicUsize::new(0),
    });
    let agent = Agent::new(
        OpenAiClient::new("test", "http://127.0.0.1:1234/v1"),
        AgentSettings::default(),
        Arc::new(McpPool::new(vec![server])),
        ToolRegistry::new(),
        UserPolicy::default(),
        handler.clone(),
    );
    let items = (0..3)
        .map(|index| {
            json!({
                "type": "mcp_approval_request",
                "id": format!("req_{index}"),
                "server_label": "github",
                "name": "list_issues",
                "arguments": "{}"
            })
        })
        .collect::<Vec<_>>();
    let (continuation, _) = with_scope(&active, async |scope| {
        agent.handle_output_items(&items, scope, None).await
    })
    .await
    .unwrap();
    assert_eq!(continuation.len(), 3);
    assert!(continuation.iter().all(|item| item["approve"] == true));
    assert_eq!(handler.max_in_flight.load(Ordering::SeqCst), 1);
}

fn remote_server(label: &str) -> McpServerConfig {
    McpServerConfig {
        label: label.into(),
        transport: McpTransport::Responses,
        url: Some("https://example.test/mcp".into()),
        tunnel_id: None,
        command: None,
        args: vec![],
        cwd: None,
        env_vars: Default::default(),
        description: None,
        authorization_env: None,
        oauth: false,
        oauth_scopes: None,
        allowed_tools: None,
        disabled_tools: vec![],
        tool_catalog: None,
        require_approval: McpApprovalMode::Always,
        reuse_connection: true,
    }
}

#[test]
fn filters_allowed_mcp_tools_for_a_user() {
    let server = McpServerConfig {
        allowed_tools: Some(vec!["list_issues".into(), "delete_issue".into()]),
        require_approval: McpApprovalMode::Never,
        ..remote_server("github")
    };
    let policy = UserPolicy::new(vec!["mcp:github:delete_issue".into()], None);

    let value = responses_mcp_tool(
        &server,
        &policy,
        &["list_issues".into(), "delete_issue".into()],
    )
    .unwrap()
    .unwrap();
    assert_eq!(value["allowed_tools"], serde_json::json!(["list_issues"]));
    assert_eq!(value["require_approval"], "never");
}

#[test]
fn selected_mcp_tool_is_the_only_tool_sent_to_the_endpoint() {
    let server = McpServerConfig {
        tool_catalog: Some(vec![
            McpToolCatalog {
                name: "search".into(),
                description: Some("Search docs".into()),
            },
            McpToolCatalog {
                name: "delete".into(),
                description: Some("Delete docs".into()),
            },
        ]),
        ..remote_server("docs")
    };
    let value = responses_mcp_tool(&server, &UserPolicy::default(), &["search".to_string()])
        .unwrap()
        .unwrap();

    assert_eq!(value["allowed_tools"], serde_json::json!(["search"]));
    assert_eq!(value["require_approval"], "always");
}

fn approval_agent(registry: ToolRegistry, approval: Arc<dyn ApprovalHandler>) -> Agent {
    Agent::new(
        OpenAiClient::new("test", "http://127.0.0.1:1234/v1"),
        AgentSettings::default(),
        Arc::new(McpPool::new(Vec::new())),
        registry,
        UserPolicy::default(),
        approval,
    )
}

#[tokio::test]
async fn local_tools_that_require_approval_run_only_when_approved() {
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = ToolRegistry::new();
    let counter = Arc::clone(&runs);
    registry
        .register(
            ToolDefinition::new(
                "deploy",
                "Deploy the site",
                json!({"type": "object", "properties": {}}),
            )
            .with_approval(),
            move |_arguments| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"deployed": true}))
                }
            },
        )
        .unwrap();
    let active = selected_local_tools(&["deploy"]);

    let denying = Arc::new(ExplainingApproval {
        seen: Mutex::new(Vec::new()),
    });
    let agent = approval_agent(registry.clone(), denying.clone());
    let (output, _) = with_scope(&active, async |scope| {
        agent
            .handle_function_call("deploy", &json!({}), scope)
            .await
    })
    .await
    .unwrap();
    assert_eq!(output["error"], "approval_denied");
    assert!(output["message"]
        .as_str()
        .unwrap()
        .contains("not part of the request"));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    let seen = denying.seen.lock().unwrap()[0].clone();
    assert_eq!(seen.source, ApprovalSource::LocalTool);
    assert_eq!(seen.target(), "deploy");
    assert_eq!(seen.tool_description.as_deref(), Some("Deploy the site"));

    let agent = approval_agent(registry, Arc::new(AlwaysApprove));
    let (output, _) = with_scope(&active, async |scope| {
        agent
            .handle_function_call("deploy", &json!({}), scope)
            .await
    })
    .await
    .unwrap();
    assert_eq!(output["deployed"], true);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn exec_is_neither_offered_nor_approved_without_allow_exec() {
    let registry = ToolRegistry::new();
    register_builtin_tools(&registry).unwrap();
    let approval = Arc::new(ExplainingApproval {
        seen: Mutex::new(Vec::new()),
    });
    let agent = approval_agent(registry, approval.clone());

    let search = |context: &ToolContext| {
        agent
            .search_tools(
                &json!({"query": "run shell command"}),
                &McpRuntime::default(),
                context,
            )
            .unwrap()
            .active
            .local
    };
    assert!(!search(&ToolContext::default()).contains("workspace_exec"));
    let enabled = ToolContext {
        allow_exec: true,
        ..ToolContext::default()
    };
    assert!(search(&enabled).contains("workspace_exec"));

    let active = selected_local_tools(&["workspace_exec"]);
    let (output, _) = with_scope(&active, async |scope| {
        agent
            .handle_function_call("workspace_exec", &json!({"command": "true"}), scope)
            .await
    })
    .await
    .unwrap();
    assert_eq!(output["error"], "tool_unavailable");
    assert!(approval.seen.lock().unwrap().is_empty());
}

fn with_usage(mut response: Value, total: u64) -> Value {
    response["usage"] =
        json!({"input_tokens": total - 5, "output_tokens": 5, "total_tokens": total});
    response
}

fn delegate_response(id: &str, task: &str) -> Value {
    json!({
        "id": id,
        "status": "completed",
        "output": [{
            "type": "function_call",
            "call_id": format!("{id}_delegate"),
            "name": "delegate_task",
            "arguments": json!({"task": task}).to_string()
        }]
    })
}

#[tokio::test]
async fn delegated_tasks_run_in_a_fresh_sub_agent_and_return_its_report() {
    let server = mock_responses(vec![
        with_usage(delegate_response("parent", "Survey the files"), 15),
        with_usage(text_response("sub", "Found three files"), 25),
        with_usage(text_response("done", "All done"), 35),
    ])
    .await;
    let mut agent = agent(ToolRegistry::new(), Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    agent.settings.max_total_tokens = Some(1000);

    let result = agent.run(request()).await.unwrap();

    assert_eq!(result.text, "All done");
    assert_eq!(result.usage.total_tokens, 75);
    assert_eq!(result.usage.responses, 3);
    assert!(result.events.iter().any(|event| matches!(
        event,
        AgentEvent::SubagentStarted { task, .. } if task == "Survey the files"
    )));
    assert!(result.events.iter().any(|event| matches!(
        event,
        AgentEvent::SubagentFinished { usage, error: None, .. } if usage.total_tokens == 25
    )));
    let requests = server.requests.lock().unwrap();
    let sub = &requests[1];
    assert!(sub["instructions"]
        .as_str()
        .unwrap()
        .contains("You are a sub-agent"));
    assert!(sub["input"].to_string().contains("Survey the files"));
    assert!(!sub["input"].to_string().contains("Do the requested work"));
    assert!(sub["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool["name"] != "delegate_task"));
    let report: Value =
        serde_json::from_str(requests[2]["input"][0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(report["report"], "Found three files");
    assert_eq!(report["outcome"], "completed");
}

#[tokio::test]
async fn sub_agents_run_on_the_model_of_their_role() {
    let parent = mock_responses(vec![
        delegate_response("parent", "Survey the files"),
        text_response("done", "All done"),
    ])
    .await;
    let quick = mock_responses(vec![text_response("sub", "Found three files")]).await;
    let mut agent = agent(ToolRegistry::new(), Vec::new()).with_subagent_models(SubagentModels {
        delegate: Some(ModelTarget {
            client: Arc::new(OpenAiClient::new("test", &quick.url)),
            model: "quick-model".into(),
            reasoning_effort: Some("low".into()),
            context_window: None,
        }),
        review: None,
    });
    agent.client = Arc::new(OpenAiClient::new("test", &parent.url));
    agent.settings.model = "main-model".into();
    agent.settings.reasoning_effort = Some("high".into());
    agent.settings.reasoning_summary = Some("auto".into());

    let result = agent.run(request()).await.unwrap();

    assert_eq!(result.text, "All done");
    assert!(result.events.iter().any(|event| matches!(
        event,
        AgentEvent::SubagentStarted { model, .. } if model == "quick-model"
    )));
    let main = parent.requests.lock().unwrap();
    assert_eq!(main.len(), 2);
    assert!(main.iter().all(|request| request["model"] == "main-model"
        && request["reasoning"] == json!({"effort": "high", "summary": "auto"})));
    let sub = quick.requests.lock().unwrap();
    assert_eq!(sub.len(), 1);
    assert_eq!(sub[0]["model"], "quick-model");
    // The summary setting is the agent's; the effort is the role's.
    assert_eq!(
        sub[0]["reasoning"],
        json!({"effort": "low", "summary": "auto"})
    );
}

#[test]
fn each_kind_of_run_uses_the_model_of_its_role() {
    let review = ModelTarget {
        client: Arc::new(OpenAiClient::new("test", "http://127.0.0.1:9/v1")),
        model: "review-model".into(),
        reasoning_effort: None,
        context_window: None,
    };
    let agent = agent(ToolRegistry::new(), Vec::new()).with_subagent_models(SubagentModels {
        delegate: None,
        review: Some(review),
    });
    let model = |depth, reviewer| {
        agent
            .target(&RunOrigin {
                depth,
                reviewer,
                ..RunOrigin::default()
            })
            .model
            .to_string()
    };
    assert_eq!(model(0, false), agent.settings.model);
    // Without a delegate model, sub-agents run on the agent's own.
    assert_eq!(model(1, false), agent.settings.model);
    assert_eq!(model(1, true), "review-model");
}

#[tokio::test]
async fn sub_agents_share_the_parent_token_budget() {
    let server = mock_responses(vec![
        with_usage(delegate_response("parent", "Survey the files"), 15),
        with_usage(text_response("sub", "Partial survey"), 25),
    ])
    .await;
    let mut agent = agent(ToolRegistry::new(), Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    agent.settings.max_total_tokens = Some(20);

    let result = agent.run(request()).await.unwrap();

    assert_eq!(result.stop_reason, StopReason::TokenLimit);
    assert_eq!(result.usage.total_tokens, 40);
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn sub_agents_cannot_delegate_further() {
    let agent = agent(ToolRegistry::new(), Vec::new());
    let context = ToolContext::default();
    let runtime = McpRuntime::default();
    let events = EventLog::new(None);
    let plan = Mutex::new(TaskPlan::default());
    let delegated_usage = Mutex::new(UsageSummary::default());
    let active = ActiveTools::default();
    let scope = RoundScope {
        conversation: None,
        call_id: None,
        round: 0,
        user_request: "",
        tool_context: &context,
        active: &active,
        mcp_runtime: &runtime,
        events: &events,
        plan: &plan,
        depth: 1,
        read_only: false,
        token_budget: None,
        delegated_usage: &delegated_usage,
    };
    let (output, _) = agent
        .handle_function_call("delegate_task", &json!({"task": "More"}), scope)
        .await
        .unwrap();
    assert_eq!(output["error"], "tool_disabled");
}

#[tokio::test]
async fn text_listener_streams_the_callers_messages_but_not_sub_agents() {
    let server = mock_responses(vec![
        delegate_response("parent", "Survey the files"),
        text_response("sub", "Found three files"),
        text_response("done", "All done"),
    ])
    .await;
    let streamed = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&streamed);
    let mut agent =
        agent(ToolRegistry::new(), Vec::new()).with_text_listener(Arc::new(move |delta| {
            sink.lock().unwrap().push(match delta {
                crate::application::ports::ResponseDelta::Text(text) => text.to_string(),
                crate::application::ports::ResponseDelta::MessageDone => "<done>".into(),
                crate::application::ports::ResponseDelta::Reasoning(_) => "<reasoning>".into(),
            })
        }));
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));

    let result = agent.run(request()).await.unwrap();

    assert_eq!(result.text, "All done");
    assert!(result.streamed);
    assert_eq!(*streamed.lock().unwrap(), ["All done", "<done>"]);
    // The endpoint was asked to stream only the caller's requests.
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests[0]["stream"], true);
    assert!(requests[1].get("stream").is_none());
    assert_eq!(requests[2]["stream"], true);
}

#[tokio::test]
async fn large_tool_outputs_are_cut_to_head_and_tail() {
    let registry = ToolRegistry::new();
    registry
        .register(
            ToolDefinition::new(
                "dump",
                "Return a lot of text",
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
            ),
            |_arguments| async move { Ok(json!({"text": format!("BEGIN{}END", "あ".repeat(10_000))})) },
        )
        .unwrap();
    let mut agent = agent(registry, Vec::new());
    agent.settings.max_tool_output_bytes = 4096;
    let item = json!({"type":"function_call","call_id":"c1","name":"dump","arguments":"{}"});
    let (continuation, _) = with_scope(&selected_local_tools(&["dump"]), async |scope| {
        agent
            .handle_output_items(std::slice::from_ref(&item), scope, None)
            .await
            .unwrap()
    })
    .await;
    let output = continuation[0]["output"].as_str().unwrap();
    assert!(output.len() < 4096 * 2, "{}", output.len());
    let output: Value = serde_json::from_str(output).unwrap();
    assert_eq!(output["truncated"], true);
    assert!(output["original_bytes"].as_u64().unwrap() > 30_000);
    assert!(output["head"].as_str().unwrap().contains("BEGIN"));
    assert!(output["tail"].as_str().unwrap().contains("END"));
}

#[tokio::test]
async fn a_user_goal_keeps_the_run_going_until_its_criteria_are_verified() {
    let verify = json!({
        "id": "verify", "status": "completed",
        "output": [{
            "type": "function_call", "call_id": "plan1", "name": "task_plan",
            "arguments": json!({"expected_revision": 1, "explanation": null, "steps": null,
                "goal": {"objective": "Tests pass", "acceptance": [
                    {"id": "c1", "description": "cargo test succeeds", "status": "met", "evidence": "42 passed"}
                ]}}).to_string()
        }]
    });
    let server = mock_responses(vec![
        text_response("claim", "Done."),
        verify,
        text_response("done", "Verified: 42 tests passed."),
    ])
    .await;
    let mut agent = agent(ToolRegistry::new(), Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    let goal = TaskGoal::from_user("Tests pass").unwrap();

    let result = agent
        .run(RunRequest::new(Vec::new()).with_goal(goal))
        .await
        .unwrap();

    assert_eq!(result.text, "Verified: 42 tests passed.");
    assert_eq!(result.outcome, RunOutcome::Completed);
    let goal = result.plan.goal.unwrap();
    assert_eq!(goal.acceptance[0].evidence.as_deref(), Some("42 passed"));
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let notice = requests[0]["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(notice.starts_with("Goal set by the user"));
    assert!(notice.contains("Objective: Tests pass\n"));
    assert!(notice.contains("define concrete, checkable acceptance criteria"));
    // Claiming completion without verification is not accepted.
    let continuation = requests[1]["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(continuation.contains("It has no acceptance criteria"));
}

#[tokio::test]
async fn an_unverified_goal_ends_incomplete_at_the_round_limit() {
    let server = mock_responses(vec![
        text_response("claim", "Done."),
        text_response("final", "Could not verify."),
    ])
    .await;
    let mut agent = agent(ToolRegistry::new(), Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    agent.settings.max_tool_rounds = 2;
    let goal = TaskGoal::from_user("Docs are updated").unwrap();

    let result = agent
        .run(RunRequest::new(vec![InputPart::Text("Update the docs".into())]).with_goal(goal))
        .await
        .unwrap();

    assert_eq!(result.outcome, RunOutcome::Incomplete);
    assert_eq!(result.stop_reason, StopReason::RoundLimit);
    let requests = server.requests.lock().unwrap();
    let notice = requests[0]["input"][0]["content"][1]["text"]
        .as_str()
        .unwrap();
    assert!(notice.contains("Objective: Docs are updated"));
    assert!(requests[1]["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("It has no acceptance criteria"));
}

#[test]
fn the_continuation_notice_names_unverified_criteria_and_open_steps() {
    let mut plan = TaskPlan::default().for_user_goal(TaskGoal::from_user("Tests pass").unwrap());
    plan.apply(&json!({"expected_revision": 1, "explanation": null,
        "steps": [{"id": "fix", "description": "Fix", "status": "in_progress", "detail": null}],
        "goal": {"objective": "Tests pass", "acceptance": [
            {"id": "tests", "description": "cargo test succeeds", "status": "pending", "evidence": null},
            {"id": "lint", "description": "clippy is clean", "status": "met", "evidence": "no warnings"}
        ]}}))
        .unwrap();
    let notice = super::continuation_notice(&plan);
    assert!(notice.contains("not verified: tests (cargo test succeeds)."));
    assert!(!notice.contains("lint"));
    assert!(notice.contains("pending or in_progress steps"));
}

#[tokio::test]
async fn repeating_one_tool_asks_the_model_to_change_approach() {
    let repeat =
        |id: &str| json!({"id": id, "status": "completed", "output": calls(&["record_action"])});
    let server = mock_responses(vec![
        search_response("search", "record_action"),
        repeat("r1"),
        repeat("r2"),
        repeat("r3"),
        text_response("done", "Stopped repeating"),
    ])
    .await;
    let (registry, count) = counting_registry();
    let mut agent = agent(registry, Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));

    let result = agent.run(request()).await.unwrap();

    assert_eq!(result.text, "Stopped repeating");
    assert_eq!(count.load(Ordering::SeqCst), 3);
    let requests = server.requests.lock().unwrap();
    let is_notice = |item: &Value| {
        item["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("Runtime notice: you have called record_action"))
    };
    // Only the third call in a row gets the notice, after its output.
    for request in &requests[..4] {
        assert!(!request["input"].as_array().unwrap().iter().any(is_notice));
    }
    let input = requests[4]["input"].as_array().unwrap();
    assert_eq!(input[0]["type"], "function_call_output");
    assert!(is_notice(&input[1]));
}

#[test]
fn the_repetition_notice_names_unloaded_tools_of_the_server() {
    let unloaded = vec!["tavily_extract".to_string(), "tavily_crawl".to_string()];
    let notice = super::repetition_notice("mcp__web__tavily_search", 3, Some("web"), &unloaded);
    assert!(notice.contains("in 3 consecutive steps"));
    assert!(notice
        .contains("Tools on server 'web' that are not loaded now: tavily_extract, tavily_crawl."));
    assert!(!super::repetition_notice("record_action", 3, None, &[]).contains("not loaded now"));
}

#[test]
fn the_repetition_notice_names_unloaded_local_tools_like_the_repeated_one() {
    let registry = ToolRegistry::new();
    register_builtin_tools(&registry).unwrap();
    let agent = agent(registry, Vec::new());
    let context = ToolContext {
        workspace: Some(std::env::temp_dir()),
        ..ToolContext::default()
    };
    let active = selected_local_tools(&["workspace_search", "workspace_find"]);
    let unloaded = agent.unloaded_local_siblings("workspace_search", &active, &context);
    assert!(
        unloaded.contains(&"workspace_read".to_string()),
        "{unloaded:?}"
    );
    assert!(!unloaded.contains(&"workspace_find".to_string()));
    assert!(unloaded.iter().all(|name| name.starts_with("workspace_")));
    // A tool that cannot run here is not suggested.
    assert!(!unloaded.contains(&"workspace_exec".to_string()));
    let notice = super::repetition_notice("workspace_search", 3, None, &unloaded);
    assert!(notice.contains("Local tools like it that are not loaded now: "));
    assert!(notice.contains("load it with tool_search first"));
}

#[test]
fn the_context_is_full_when_the_next_request_may_not_fit() {
    use super::context_full;
    assert!(!context_full(None, Some(1_000_000)));
    assert!(!context_full(Some(100_000), None));
    // A fifth of the window, and at least 24k tokens, is left for the next
    // request.
    assert!(!context_full(Some(262_144), Some(200_000)));
    assert!(context_full(Some(262_144), Some(210_000)));
    assert!(!context_full(Some(100_000), Some(75_000)));
    assert!(context_full(Some(100_000), Some(76_000)));
    // A small window keeps a third of it.
    assert!(!context_full(Some(32_768), Some(21_000)));
    assert!(context_full(Some(32_768), Some(22_000)));
}

#[test]
fn context_overflows_are_told_from_other_errors() {
    use super::is_context_overflow;
    for error in [
        "Responses API request failed (400 Bad Request): context_length_exceeded",
        "This model's maximum context length is 8192 tokens",
        "Trying to keep the first 9000 tokens when context the overflows.",
        "Your input exceeds the context window of this model.",
    ] {
        assert!(is_context_overflow(error), "{error}");
    }
    assert!(!is_context_overflow("rate limit reached"));
    assert!(!is_context_overflow("invalid tool arguments"));
}

#[tokio::test]
async fn the_history_is_compacted_before_it_fills_the_context_window() {
    let server = mock_responses(vec![
        with_usage(search_response("full", "record_action"), 90_000),
        text_response("summary", "Loaded record_action"),
        text_response("done", "Finished"),
    ])
    .await;
    let (registry, _) = counting_registry();
    let mut agent = agent(registry, Vec::new()).with_context_window(Some(100_000));
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));

    let result = agent.run(request()).await.unwrap();

    assert_eq!(result.text, "Finished");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[1]["instructions"]
        .as_str()
        .unwrap()
        .starts_with("You compact the history"));
    assert!(requests[2]["input"]
        .to_string()
        .contains("Loaded record_action"));
}

#[tokio::test]
async fn a_request_over_the_context_is_compacted_and_sent_again() {
    let server = mock_responses(vec![
        json!({"error": {"message": "This model's maximum context length is 8192 tokens"}}),
        text_response("summary", "The user asked for the work"),
        text_response("done", "Finished"),
    ])
    .await;
    let mut agent = agent(ToolRegistry::new(), Vec::new()).with_context_window(Some(1_000_000));
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));

    let result = agent.run(request()).await.unwrap();

    assert_eq!(result.text, "Finished");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[1]["instructions"]
        .as_str()
        .unwrap()
        .starts_with("You compact the history"));
    assert!(result.events.iter().any(|event| matches!(
        event,
        AgentEvent::AssistantProgress { text, .. } if text.contains("exceeded the model's context")
    )));
}

#[tokio::test]
async fn repeating_after_the_notices_stops_the_run_with_a_report() {
    let repeat =
        |id: &str| json!({"id": id, "status": "completed", "output": calls(&["record_action"])});
    let mut responses = vec![search_response("search", "record_action")];
    responses.extend((1..=9).map(|index| repeat(&format!("r{index}"))));
    responses.push(text_response("report", "Stuck on record_action"));
    let server = mock_responses(responses).await;
    let (registry, count) = counting_registry();
    let mut agent = agent(registry, Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));

    let result = agent.run(request()).await.unwrap();

    assert_eq!(result.text, "Stuck on record_action");
    assert_eq!(result.stop_reason, StopReason::NoProgress);
    assert_eq!(count.load(Ordering::SeqCst), 9);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 11);
    let last = &requests[10];
    assert_eq!(last["tools"], json!([]));
    assert!(last["instructions"]
        .as_str()
        .unwrap()
        .contains("kept repeating"));
}

#[test]
fn a_tool_search_or_another_tool_resets_the_repetition() {
    let round = |names: &[&str]| calls(names);
    let mut repetition = super::Repetition::default();
    assert_eq!(repetition.observe(&round(&["a"])), None);
    assert_eq!(repetition.observe(&round(&["a", "task_plan"])), None);
    assert_eq!(repetition.observe(&round(&["tool_search"])), None);
    assert_eq!(repetition.observe(&round(&["a"])), None);
    assert_eq!(repetition.observe(&round(&["a"])), None);
    assert_eq!(repetition.observe(&round(&["a", "b"])), None);
    assert_eq!(repetition.observe(&round(&["a"])), None);
    assert_eq!(repetition.observe(&round(&["a"])), None);
    assert_eq!(repetition.observe(&round(&["a"])), Some(3));
    assert_eq!(repetition.observe(&round(&["a"])), None);
    assert_eq!(repetition.observe(&round(&["a"])), None);
    assert_eq!(repetition.observe(&round(&["a"])), Some(6));
}

#[test]
fn reading_other_files_in_turn_is_not_a_repetition() {
    let read = |path: &str| {
        vec![
            json!({"type": "function_call", "call_id": format!("read_{path}"),
            "name": "workspace_read", "arguments": json!({"path": path}).to_string()}),
        ]
    };
    let mut repetition = super::Repetition::default();
    for path in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"] {
        assert_eq!(repetition.observe(&read(path)), None, "{path}");
    }
    // Reading the same thing again does not make progress.
    assert_eq!(repetition.observe(&read("f.rs")), None);
    assert_eq!(repetition.observe(&read("f.rs")), Some(3));
    // Editing or running one thing after another is progress too.
    let call = |name: &str, arguments: Value| {
        vec![
            json!({"type": "function_call", "call_id": "call", "name": name,
            "arguments": arguments.to_string()}),
        ]
    };
    let mut repetition = super::Repetition::default();
    for path in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        let edit = call("workspace_edit", json!({"path": path}));
        assert_eq!(repetition.observe(&edit), None, "{path}");
    }
    let test = call("workspace_exec", json!({"command": "cargo test"}));
    assert_eq!(repetition.observe(&test), None);
    assert_eq!(repetition.observe(&test), None);
    assert_eq!(repetition.observe(&test), Some(3));
    // Searches with other queries still count.
    let mut repetition = super::Repetition::default();
    for query in ["a", "b"] {
        let search = call("workspace_search", json!({"query": query}));
        assert_eq!(repetition.observe(&search), None);
    }
    let search = call("workspace_search", json!({"query": "c"}));
    assert_eq!(repetition.observe(&search), Some(3));
}

#[test]
fn calling_only_task_plan_repeatedly_gets_a_notice() {
    let round = |names: &[&str]| calls(names);
    let mut repetition = super::Repetition::default();
    assert_eq!(repetition.observe(&round(&["task_plan"])), None);
    assert_eq!(repetition.observe(&round(&["task_plan"])), None);
    assert_eq!(repetition.observe(&round(&["task_plan"])), Some(3));
    // Work between plan updates ends the streak.
    assert_eq!(
        repetition.observe(&round(&["web_fetch", "task_plan"])),
        None
    );
    assert_eq!(repetition.observe(&round(&["task_plan"])), None);
    let notice = super::repetition_notice("task_plan", 3, None, &[]);
    assert!(notice.contains("called only task_plan in 3 consecutive steps"));
    assert!(notice.contains("were not errors"));
}

/// Read every raw history event under `dir`, whatever its sync state.
fn journal_events(dir: &std::path::Path) -> Vec<Value> {
    let mut events = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            events.extend(journal_events(&path));
        } else if path.extension().is_some_and(|e| e == "json")
            && path.file_stem().unwrap().len() == 20
        {
            events.push(serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap());
        }
    }
    events
}

#[tokio::test]
async fn raw_history_does_not_change_requests_without_a_session() {
    let server = mock_responses(vec![
        search_response("search", "anything"),
        text_response("done", "Finished."),
    ])
    .await;
    let root = tempfile::tempdir().unwrap();
    let history = Arc::new(
        crate::infrastructure::chronotope::Chronotope::new(
            crate::infrastructure::chronotope::HistorySettings {
                enabled: true,
                // Nothing listens here; the run keeps the events for a later sync.
                base_url: "http://127.0.0.1:9".into(),
                data_dir: root.path().join("history"),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let mut agent = agent(ToolRegistry::new(), Vec::new())
        .with_history(Arc::clone(&history) as Arc<dyn crate::application::ports::HistoryBackend>);
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    // The goal cannot be verified, so the second round is the final one.
    agent.settings.max_tool_rounds = 2;
    let mut request = RunRequest::new(vec![InputPart::Text("Update the docs".into())])
        .with_goal(TaskGoal::from_user("Docs are updated").unwrap());
    request.context.user_id = "alice".into();

    agent.run(request).await.unwrap();

    let requests = server.requests.lock().unwrap();
    // The same shape as a run without history: one user message carrying the
    // goal notice, then continuation by previous_response_id.
    assert_eq!(requests[0]["input"].as_array().unwrap().len(), 1);
    assert!(requests[0]["input"][0]["content"][1]["text"]
        .as_str()
        .unwrap()
        .contains("Objective: Docs are updated"));
    assert!(requests[0].get("store").is_none());
    assert_eq!(requests[1]["previous_response_id"], "search");
    assert_eq!(requests[1]["input"].as_array().unwrap().len(), 1);
    drop(requests);

    assert!(
        history.status("alice").unwrap()["pending_events"]
            .as_u64()
            .unwrap()
            > 0
    );
    let events = journal_events(&root.path().join("history"));
    let human = events
        .iter()
        .filter(|e| e["origin"] == "human")
        .map(|e| e["content"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(human, ["Update the docs", "Docs are updated"]);
    assert!(events.iter().any(|e| e["origin"] == "runtime"
        && e["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("Goal set by the user"))));
}

#[tokio::test]
async fn a_new_turn_keeps_the_tools_loaded_in_earlier_turns() {
    let server = mock_responses(vec![
        search_response("search", "record_action"),
        text_response("first", "Found it."),
        json!({"id": "act", "status": "completed", "output": calls(&["record_action"])}),
        text_response("second", "Recorded."),
    ])
    .await;
    let (registry, count) = counting_registry();
    let mut agent = agent(registry, Vec::new());
    agent.client = Arc::new(OpenAiClient::new("test", &server.url));
    let binding = SessionBinding::new(&ToolContext::default(), &server.url).unwrap();
    let mut store = crate::infrastructure::memory_store::MemoryConversation::new(binding);

    agent.run_in_session(request(), &mut store).await.unwrap();
    let result = agent.run_in_session(request(), &mut store).await.unwrap();

    assert_eq!(result.text, "Recorded.");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let requests = server.requests.lock().unwrap();
    // The second turn starts with the tool the first turn's search loaded.
    assert!(requests[2]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "record_action"));
}

#[test]
fn tool_selection_is_restored_from_the_last_search_only() {
    let search = |id: &str, tools: Value| {
        [
            json!({"type":"function_call","call_id":id,"name":"tool_search","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":id,"output":json!({"tools":tools}).to_string()}),
        ]
    };
    let mut history = Vec::new();
    history.extend(search("old", json!([{"kind":"local","name":"old_tool"}])));
    history.extend(search(
        "new",
        json!([
            {"kind":"local","name":"workspace_read"},
            {"kind":"mcp","name":"search","server_label":"web","function_name":"mcp__web__search"},
            {"kind":"mcp","name":"list","server_label":"remote"}
        ]),
    ));
    // Another tool's output is not a selection, even with a `tools` field.
    history.push(json!({"type":"function_call","call_id":"other","name":"echo","arguments":"{}"}));
    history.push(json!({"type":"function_call_output","call_id":"other","output":json!({"tools":[{"name":"x"}]}).to_string()}));

    let active = ActiveTools::from_history(&history);

    assert_eq!(active.local, ["workspace_read".to_string()].into());
    assert_eq!(active.direct_mcp, ["mcp__web__search".to_string()].into());
    assert_eq!(active.responses_mcp["remote"], ["list"]);
    assert!(ActiveTools::from_history(&[]).local.is_empty());
}
