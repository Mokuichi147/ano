//! `ano acp` speaks the Agent Client Protocol on stdin and stdout, as an
//! editor starts it.

use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
};

const TIMEOUT: Duration = Duration::from_secs(20);

/// The editor's side of the connection.
struct Editor {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl Editor {
    /// Start `ano acp` in `directory`, with a config of its own there.
    fn start(directory: &Path, endpoint: &str, args: &[&str]) -> Self {
        Self::start_with(directory, endpoint, "", args)
    }

    /// Start `ano acp` with `extra` added to its config.
    fn start_with(directory: &Path, endpoint: &str, extra: &str, args: &[&str]) -> Self {
        std::fs::create_dir_all(directory.join(".ano")).unwrap();
        std::fs::write(
            directory.join(".ano/config.toml"),
            format!("[agent]\nmodel = 'test-model'\n{extra}"),
        )
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_ano"))
            .current_dir(directory)
            .env("OPENAI_BASE_URL", endpoint)
            .env("OPENAI_API_KEY", "test-fixture-key")
            .arg("acp")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    async fn write(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    /// The next message ano sends. Every line of stdout is a JSON-RPC
    /// message.
    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(TIMEOUT, self.stdout.next_line())
            .await
            .expect("ano did not answer in time")
            .unwrap()
            .expect("ano closed stdout");
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON-RPC: {line}"))
    }

    /// The messages until the response to `id`, and the response.
    async fn until_response(&mut self, id: u64) -> (Vec<Value>, Value) {
        let mut before = Vec::new();
        loop {
            let message = self.next().await;
            if message["id"] == id && message.get("method").is_none() {
                return (before, message);
            }
            before.push(message);
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.request(method, params).await;
        let (_, response) = self.until_response(id).await;
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }
}

/// A Responses API that answers each request with the next of `outputs`,
/// and the requests it got.
async fn responses(outputs: Vec<Value>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let outputs = Arc::new(outputs);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let (requests, outputs) = (Arc::clone(&captured), Arc::clone(&outputs));
            async move {
                let mut requests = requests.lock().unwrap();
                requests.push(payload);
                let output = outputs
                    .get(requests.len() - 1)
                    .cloned()
                    .unwrap_or_else(|| message("done"));
                Json(json!({"id": format!("resp_{}", requests.len()), "status": "completed", "output": output}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (endpoint, requests)
}

fn message(text: &str) -> Value {
    json!([{"type": "message", "content": [{"type": "output_text", "text": text}]}])
}

/// `workspace_exec` is offered once `tool_search` has found it.
fn search_shell() -> Value {
    json!([{"type": "function_call", "call_id": "search", "name": "tool_search",
        "arguments": "{\"query\":\"shell command\"}"}])
}

fn exec_call(id: &str, command: &str) -> Value {
    json!([{"type": "function_call", "call_id": id, "name": "workspace_exec",
        "arguments": json!({"command": command}).to_string()}])
}

fn updates(messages: &[Value]) -> Vec<&Value> {
    messages
        .iter()
        .filter(|message| message["method"] == "session/update")
        .map(|message| &message["params"]["update"])
        .collect()
}

async fn open_session(editor: &mut Editor, workspace: &Path) -> (String, Value) {
    let initialized = editor
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    assert_eq!(initialized["protocolVersion"], 1);
    assert_eq!(initialized["agentInfo"]["name"], "ano");
    let prompts = &initialized["agentCapabilities"]["promptCapabilities"];
    assert_eq!(prompts["image"], true);
    assert_eq!(prompts["embeddedContext"], true);

    let session = editor
        .call("session/new", json!({"cwd": workspace, "mcpServers": []}))
        .await;
    (session["sessionId"].as_str().unwrap().to_string(), session)
}

/// A command waits for the editor's permission, runs, and its output reaches
/// the editor and the model; after the editor switches the session to deny,
/// the next command is refused without asking.
#[cfg(unix)]
#[tokio::test]
async fn commands_ask_the_editor_and_follow_the_mode_it_chooses() {
    let (endpoint, requests) = responses(vec![
        search_shell(),
        exec_call("call_echo", "echo from-acp"),
        message("It printed from-acp."),
        search_shell(),
        exec_call("call_again", "echo again"),
        message("It was denied."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let mut editor = Editor::start(
        &workspace,
        &endpoint,
        &["--allow-exec", "--approval-mode", "ask"],
    );
    let (session_id, session) = open_session(&mut editor, &workspace).await;
    assert_eq!(session["modes"]["currentModeId"], "ask");
    let approval = session["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == "approval_mode")
        .unwrap();
    assert_eq!(approval["currentValue"], "ask");

    let prompt = editor
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Run echo"}]}),
        )
        .await;
    // ano asks before the command runs.
    let mut before = Vec::new();
    let asked = loop {
        let message = editor.next().await;
        if message["method"] == "session/request_permission" {
            break message;
        }
        before.push(message);
    };
    let call = updates(&before)
        .into_iter()
        .find(|update| update["sessionUpdate"] == "tool_call" && update["kind"] == "execute")
        .unwrap()
        .clone();
    assert_eq!(call["toolCallId"], "call_echo");
    assert_eq!(call["kind"], "execute");
    assert_eq!(call["title"], "workspace_exec: echo from-acp");
    assert_eq!(asked["params"]["sessionId"], session_id);
    assert_eq!(asked["params"]["toolCall"]["toolCallId"], "call_echo");
    let options: Vec<&str> = asked["params"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["kind"].as_str().unwrap())
        .collect();
    assert_eq!(options, ["allow_once", "reject_once"]);
    editor
        .write(json!({"jsonrpc": "2.0", "id": asked["id"],
            "result": {"outcome": {"outcome": "selected", "optionId": "allow"}}}))
        .await;

    let (messages, response) = editor.until_response(prompt).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let shown = updates(&messages);
    let result = shown
        .iter()
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .unwrap();
    assert_eq!(result["toolCallId"], "call_echo");
    assert_eq!(result["status"], "completed");
    assert!(result["content"][0]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("from-acp"));
    let answer: String = shown
        .iter()
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .map(|update| update["content"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(answer, "It printed from-acp.");
    let output = requests.lock().unwrap()[2]["input"].clone();
    assert!(output.to_string().contains("from-acp"), "{output}");

    // The editor switches the session to deny.
    editor
        .call(
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": "deny"}),
        )
        .await;
    let prompt = editor
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Again"}]}),
        )
        .await;
    let (messages, response) = editor.until_response(prompt).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(messages
        .iter()
        .all(|message| message["method"] != "session/request_permission"));
    let denied = messages
        .iter()
        .filter(|message| message["method"] == "session/update")
        .map(|message| &message["params"]["update"])
        .find(|update| {
            update["sessionUpdate"] == "tool_call_update" && update["toolCallId"] == "call_again"
        })
        .unwrap();
    assert_eq!(denied["status"], "failed");
    // The editor's other view of the mode follows.
    let setting = updates(&messages)
        .into_iter()
        .find(|update| update["sessionUpdate"] == "config_option_update")
        .unwrap();
    assert_eq!(setting["configOptions"][0]["currentValue"], "deny");
    drop(editor.stdin);
    let status = tokio::time::timeout(TIMEOUT, editor.child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}

/// An environment's approval mode stays when the editor switches presets.
#[cfg(unix)]
#[tokio::test]
async fn an_environments_approval_mode_survives_a_preset_switch() {
    let (endpoint, requests) = responses(vec![
        search_shell(),
        exec_call("call_locked", "echo locked"),
        message("Denied."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let mut editor = Editor::start_with(
        &workspace,
        &endpoint,
        "[presets.other]\nmodel = 'other-model'\n[environments.locked]\nallow_exec = true\napproval_mode = 'deny'\n",
        &["--environment", "locked"],
    );
    let (session_id, session) = open_session(&mut editor, &workspace).await;
    // The editor cannot change the mode, only the preset.
    assert!(session.get("modes").is_none_or(Value::is_null), "{session}");
    let ids: Vec<&str> = session["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["preset"]);
    let switched = editor
        .call(
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "preset", "value": "other"}),
        )
        .await;
    assert_eq!(switched["configOptions"][0]["currentValue"], "other");

    let prompt = editor
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Run it"}]}),
        )
        .await;
    let (messages, response) = editor.until_response(prompt).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(messages
        .iter()
        .all(|message| message["method"] != "session/request_permission"));
    assert!(updates(&messages)
        .iter()
        .any(|update| { update["toolCallId"] == "call_locked" && update["status"] == "failed" }));
    assert_eq!(requests.lock().unwrap()[0]["model"], "other-model");
}

/// The editor cancels a turn while it waits for permission; the turn ends as
/// cancelled and the conversation goes on.
#[cfg(unix)]
#[tokio::test]
async fn a_cancelled_turn_ends_as_cancelled_and_the_session_goes_on() {
    let (endpoint, _) = responses(vec![
        search_shell(),
        exec_call("call_wait", "echo never"),
        message("Next answer."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let mut editor = Editor::start(
        &workspace,
        &endpoint,
        &["--allow-exec", "--approval-mode", "ask"],
    );
    let (session_id, _) = open_session(&mut editor, &workspace).await;
    let prompt = editor
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Run it"}]}),
        )
        .await;
    loop {
        if editor.next().await["method"] == "session/request_permission" {
            break;
        }
    }
    editor
        .write(json!({"jsonrpc": "2.0", "method": "session/cancel",
            "params": {"sessionId": session_id}}))
        .await;
    let (messages, response) = editor.until_response(prompt).await;
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    // The call that waited does not stay running in the editor.
    assert!(updates(&messages)
        .iter()
        .any(|update| { update["toolCallId"] == "call_wait" && update["status"] == "failed" }));

    let prompt = editor
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Go on"}]}),
        )
        .await;
    let (messages, response) = editor.until_response(prompt).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(updates(&messages)
        .iter()
        .any(|update| update["content"]["text"] == "Next answer."));
}

/// A session the editor opens in a folder that does not exist is refused,
/// and so is a prompt to an unknown or closed session.
#[tokio::test]
async fn sessions_need_an_existing_folder_and_prompts_a_known_session() {
    let (endpoint, _) = responses(Vec::new()).await;
    let workspace = tempfile::tempdir().unwrap();
    let mut editor = Editor::start(workspace.path(), &endpoint, &[]);
    editor
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    let id = editor
        .request(
            "session/new",
            json!({"cwd": workspace.path().join("missing"), "mcpServers": []}),
        )
        .await;
    let (_, response) = editor.until_response(id).await;
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("does not exist"));
    let id = editor
        .request(
            "session/prompt",
            json!({"sessionId": "nope", "prompt": [{"type": "text", "text": "hi"}]}),
        )
        .await;
    let (_, response) = editor.until_response(id).await;
    assert!(response.get("error").is_some(), "{response}");

    // A closed session takes no more prompts.
    let session = editor
        .call(
            "session/new",
            json!({"cwd": std::fs::canonicalize(workspace.path()).unwrap(), "mcpServers": []}),
        )
        .await;
    let session_id = session["sessionId"].clone();
    editor
        .call("session/close", json!({"sessionId": session_id}))
        .await;
    let id = editor
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "hi"}]}),
        )
        .await;
    let (_, response) = editor.until_response(id).await;
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown session"));
}
