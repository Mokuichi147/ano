//! `ano chat`: a multi-turn conversation on the terminal.
//!
//! One thread reads stdin on request, so the prompt and MCP approval
//! questions never compete for input. On a terminal it uses a line editor
//! that knows the display width of wide characters, so editing Japanese text
//! (including IME input) redraws correctly, and ↑/↓ recall earlier prompts.
//! Ctrl+C cancels the running turn and keeps the conversation; `/exit` or
//! end of input quits.

use super::{
    output::{self, ProgressHold},
    provider::{endpoint, KnownModels},
    run::{prepare_agent, PreparedAgent},
    AgentOptions,
};
use crate::{
    application::{
        agent::{Agent, RunRequest},
        input::InputPart,
        ports::{ApprovalHandler, ConversationStore, McpApprovalRequest},
        registry::ToolRegistry,
    },
    config::{AppConfig, ModelRequest, ModelSelection, DEFAULT_PRESET},
    domain::{
        plan::TaskGoal,
        session::SessionStatus,
        skill::{SKILL_READ_NAME, SKILL_SAVE_NAME},
    },
    harness::{
        approval::ApprovalFactory,
        models::{Connections, RunModels},
    },
    infrastructure::memory_store::MemoryConversation,
};
use anyhow::Result;
use async_trait::async_trait;
use rustyline::{error::ReadlineError, DefaultEditor};
use std::{
    io::{BufRead, IsTerminal, Write},
    sync::{mpsc, Arc},
};
use tokio::sync::oneshot;

const HELP: &str = "Commands:
  /plan    show the task plan
  /goal TEXT
           set a goal and work until it is verified as reached
  /goal    show the goal;  /goal clear  clear it
  /skill [FOCUS]
           save what worked in this conversation as a skill for later runs
  /model [NAME]
           show the model, or switch to NAME on the current provider
  /models  list the models the current provider offers
  /provider [NAME [MODEL]]
           list providers, or switch to NAME (and MODEL) and continue there
  /preset [NAME]
           list presets, or switch to the provider, model, and effort of NAME
           ('default' returns to the configured ones)
  /effort [LEVEL]
           show the reasoning effort, or change it (none, minimal, low,
           medium, high, xhigh, max, ultra)
  /usage   show token usage of this conversation
  /compact compact the conversation now (summarize it to save context)
  /clear   start a new conversation (not with --session)
  /help    show this help
  /exit    quit (also Ctrl+D)
Ctrl+C cancels the running turn and keeps the conversation.";

enum Input {
    Line(String),
    /// Ctrl+C while editing a line on the terminal.
    Interrupted,
    /// Ctrl+D, end of piped input, or an unreadable terminal.
    Eof,
}

struct ReadRequest {
    prompt: String,
    /// Keep the line for ↑/↓ recall. Approval answers are not kept.
    remember: bool,
    reply: oneshot::Sender<Input>,
}

/// Reads one line per request on a dedicated thread. Nothing is read ahead,
/// so the terminal is free for progress output while a turn runs.
struct LineReader {
    requests: mpsc::Sender<ReadRequest>,
}

impl LineReader {
    fn spawn(interactive: bool) -> Arc<Self> {
        let (requests, received) = mpsc::channel::<ReadRequest>();
        // A plain thread, not a runtime blocking task: it may stay blocked on
        // stdin at exit without holding up runtime shutdown.
        std::thread::spawn(move || {
            let mut editor = if interactive {
                DefaultEditor::new().ok()
            } else {
                None
            };
            for request in received {
                let input = match editor.as_mut() {
                    Some(editor) => match editor.readline(&request.prompt) {
                        Ok(line) => {
                            if request.remember && !line.trim().is_empty() {
                                editor.add_history_entry(line.as_str()).ok();
                            }
                            Input::Line(line)
                        }
                        Err(ReadlineError::Interrupted) => Input::Interrupted,
                        Err(_) => Input::Eof,
                    },
                    None => {
                        let mut line = String::new();
                        match std::io::stdin().lock().read_line(&mut line) {
                            Ok(0) | Err(_) => Input::Eof,
                            Ok(_) => Input::Line(line.trim_end_matches(['\n', '\r']).to_string()),
                        }
                    }
                };
                // The requester may have stopped waiting; that is fine.
                request.reply.send(input).ok();
            }
        });
        Arc::new(Self { requests })
    }

    async fn read(&self, prompt: &str, remember: bool) -> Input {
        let (reply, response) = oneshot::channel();
        let request = ReadRequest {
            prompt: prompt.to_string(),
            remember,
            reply,
        };
        if self.requests.send(request).is_err() {
            return Input::Eof;
        }
        response.await.unwrap_or(Input::Eof)
    }
}

/// Asks on the terminal and reads the answer through the shared reader.
struct ChatApproval {
    lines: Arc<LineReader>,
}

#[async_trait]
impl ApprovalHandler for ChatApproval {
    async fn approve(&self, request: McpApprovalRequest) -> Result<bool> {
        // Events of tool calls running in parallel wait until the answer.
        let _hold = ProgressHold::start();
        eprintln!(
            "\n{}: {}\nArguments: {}",
            request.heading(),
            request.target(),
            request.arguments
        );
        if let Some(review) = &request.review {
            eprintln!("Automatic review: {review}");
        }
        std::io::stderr().flush().ok();
        Ok(
            match self.lines.read("Allow this call? [y/N] ", false).await {
                Input::Line(answer) => {
                    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
                }
                Input::Interrupted | Input::Eof => false,
            },
        )
    }
}

pub(super) async fn run(
    config: AppConfig,
    user_id: String,
    options: AgentOptions,
    registry: ToolRegistry,
) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    let lines = LineReader::spawn(interactive);
    let PreparedAgent {
        mut agent,
        context,
        mut binding,
        session,
        mcp,
        answer,
        mut selection,
        base,
        mut connections,
        approval,
    } = prepare_agent(
        &config,
        &user_id,
        &options,
        registry,
        Arc::new(ChatApproval {
            lines: Arc::clone(&lines),
        }),
        interactive,
        std::io::stdout()
            .is_terminal()
            .then(|| output::TextFormat::for_stdout(options.raw)),
    )?;
    let persistent = session.is_some();
    let mut store: Box<dyn ConversationStore> = match session {
        Some(session) => Box::new(session),
        None => Box::new(MemoryConversation::new(binding.clone())),
    };
    if interactive {
        eprintln!("ano chat — type /help for commands, /exit or Ctrl+D to quit.");
    }

    let result = async {
        loop {
            // The status line of the last turn gives way to the prompt.
            output::clear_status();
            let input = if interactive {
                // The line editor reads Ctrl+C itself while a line is edited.
                lines.read("> ", true).await
            } else {
                tokio::select! {
                    input = lines.read("", false) => input,
                    _ = tokio::signal::ctrl_c() => Input::Eof,
                }
            };
            let line = match input {
                Input::Line(line) => line,
                Input::Interrupted => {
                    eprintln!("(use /exit or Ctrl+D to quit)");
                    continue;
                }
                Input::Eof => break,
            };
            let prompt = line.trim();
            let mut goal = None;
            // A command that becomes the model's input instead of the line.
            let mut model_prompt = None;
            let starts_goal = prompt.strip_prefix("/goal ").is_some_and(|text| !text.trim().is_empty() && text.trim() != "clear");
            let saves_skill = config.skills.enabled && (prompt == "/skill" || prompt.starts_with("/skill "));
            if prompt.starts_with('/') && !starts_goal && !saves_skill {
                // A command runs even if its raw history cannot be saved.
                if let Err(error) = agent.record_control_input(store.as_mut(), &line).await {
                    eprintln!("warning: failed to record the command in history: {error:#}");
                }
            }
            match prompt {
                "" => continue,
                "/exit" | "/quit" => break,
                "/help" => {
                    eprintln!("{HELP}");
                    continue;
                }
                "/plan" => {
                    eprintln!("{}", output::format_plan(&store.data().plan));
                    continue;
                }
                "/clear" => {
                    if persistent {
                        eprintln!("/clear is not available with --session; start ano chat with another session file instead");
                    } else {
                        store = Box::new(MemoryConversation::new(binding.clone()));
                        eprintln!("(started a new conversation)");
                    }
                    continue;
                }
                "/model" | "/effort" => {
                    eprintln!("{} ({})", selection.describe(), agent.endpoint());
                    continue;
                }
                "/preset" => {
                    eprintln!("{}", format_presets(&config, &base, &selection));
                    continue;
                }
                "/provider" => {
                    eprintln!("{}", format_providers(&config, &selection));
                    continue;
                }
                "/models" => {
                    match agent.list_models().await {
                        Ok(models) => eprintln!("{}", format_models(&selection, models)),
                        Err(error) => eprintln!("error: {error:#}"),
                    }
                    continue;
                }
                command if ["/model ", "/provider ", "/preset ", "/effort "].iter().any(|prefix| command.starts_with(prefix)) => {
                    let mut words = command.split_whitespace();
                    let current = ModelRequest::from(&selection.choice);
                    let layers = match (words.next(), words.next(), words.next(), words.next()) {
                        (Some("/model"), Some(model), None, None) => vec![current, ModelRequest {
                            model: Some(model.to_string()),
                            ..ModelRequest::default()
                        }],
                        (Some("/provider"), Some(provider), model, None) => vec![current, ModelRequest {
                            provider: Some(provider.to_string()),
                            model: model.map(str::to_string),
                            ..ModelRequest::default()
                        }],
                        (Some("/preset"), Some(DEFAULT_PRESET), None, None) => base.clone(),
                        (Some("/preset"), Some(preset), None, None) => vec![current, ModelRequest::preset(preset)],
                        (Some("/effort"), Some(effort), None, None) => vec![current, ModelRequest {
                            reasoning_effort: Some(effort.to_string()),
                            ..ModelRequest::default()
                        }],
                        _ => {
                            eprintln!("usage: /model NAME, /provider NAME [MODEL], /preset NAME, or /effort LEVEL");
                            continue;
                        }
                    };
                    let switched = switch_model(
                        &config,
                        &layers,
                        &base,
                        &mut connections,
                        &mut agent,
                        store.as_mut(),
                        &approval,
                        &mut selection,
                    );
                    match switched {
                        Ok(()) => {
                            binding.endpoint = store.data().binding.endpoint.clone();
                            eprintln!(
                                "(switched to {} ({}))",
                                selection.describe(),
                                agent.endpoint()
                            );
                        }
                        Err(error) => eprintln!("error: {error:#}"),
                    }
                    continue;
                }
                "/usage" => {
                    let usage = &store.data().usage;
                    eprintln!(
                        "{} tokens ({} input, {} output) over {} responses",
                        usage.total_tokens,
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.responses
                    );
                    continue;
                }
                "/compact" => {
                    let compacted = tokio::select! {
                        result = agent.compact_conversation(store.as_mut()) => Some(result),
                        _ = tokio::signal::ctrl_c() => None,
                    };
                    match compacted {
                        Some(Ok(record)) => eprintln!(
                            "(compacted {} items, {} bytes -> {} items, {} bytes)",
                            record.before_items,
                            record.before_bytes,
                            record.after_items,
                            record.after_bytes
                        ),
                        Some(Err(error)) => eprintln!("error: {error:#}"),
                        None => eprintln!("\n[interrupted] the conversation was not compacted"),
                    }
                    continue;
                }
                command if command == "/goal" || command.starts_with("/goal ") => {
                    match command["/goal".len()..].trim() {
                        "" => {
                            match &store.data().plan.goal {
                                Some(goal) => eprintln!("{}", output::format_goal(goal)),
                                None => eprintln!(
                                    "(no goal; set one with /goal TEXT)"
                                ),
                            }
                            continue;
                        }
                        "clear" => {
                            if store.data().plan.goal.is_some() {
                                let plan = store.data().plan.without_goal();
                                store.replace_plan(&plan)?;
                                eprintln!("(goal cleared)");
                            } else {
                                eprintln!("(no goal to clear)");
                            }
                            continue;
                        }
                        text => match TaskGoal::from_user(text) {
                            Ok(parsed) => goal = Some(parsed),
                            Err(error) => {
                                eprintln!("error: {error:#}");
                                continue;
                            }
                        },
                    }
                }
                command if command == "/skill" || command.starts_with("/skill ") => {
                    if !saves_skill {
                        eprintln!("skills are disabled; set [skills] enabled = true in the config");
                        continue;
                    }
                    model_prompt = Some(skill_prompt(command["/skill".len()..].trim()));
                }
                command if command.starts_with('/') => {
                    eprintln!("unknown command {command}; type /help");
                    continue;
                }
                _ => {}
            }

            let request = RunRequest {
                // The goal notice states the request.
                input: if goal.is_some() {
                    Vec::new()
                } else {
                    vec![InputPart::Text(
                        model_prompt.clone().unwrap_or_else(|| prompt.to_string()),
                    )]
                },
                // The raw history keeps the line as typed, spaces included.
                raw_input: (goal.is_some() || model_prompt.is_some() || prompt != line)
                    .then(|| vec![InputPart::Text(line.clone())]),
                context: context.clone(),
                goal,
            };
            let outcome = tokio::select! {
                result = agent.run_in_session(request, store.as_mut()) => Some(result),
                _ = tokio::signal::ctrl_c() => None,
            };
            output::clear_status();
            match outcome {
                Some(Ok(result)) => {
                    let format = output::TextFormat::for_stdout(options.raw);
                    let text = output::format_result(&result, false, format)?;
                    if !text.is_empty() {
                        // A blank line sets the answer apart from the progress.
                        println!("\n{text}\n");
                    }
                }
                // The failure is recorded in the conversation; the next turn
                // can continue from it.
                Some(Err(error)) => eprintln!("error: {error:#}"),
                None => {
                    if let Some(answer) = &answer {
                        answer.abandon();
                    }
                    if store.data().status == SessionStatus::Running {
                        store.fail("Interrupted by the user.")?;
                    }
                    eprintln!("\n[interrupted] completed actions are not undone");
                }
            }
        }
        anyhow::Ok(())
    }
    .await;
    mcp.shutdown().await;
    result
}

/// Continue the conversation in `store` with the model `layers` choose. The
/// roles follow the new model; `base` is what their `default` preset stands
/// for. Nothing changes when any step fails.
#[allow(clippy::too_many_arguments)]
fn switch_model(
    config: &AppConfig,
    layers: &[ModelRequest],
    base: &[ModelRequest],
    connections: &mut Connections,
    agent: &mut Agent,
    store: &mut dyn ConversationStore,
    approval: &ApprovalFactory,
    selection: &mut ModelSelection,
) -> Result<()> {
    let next = config.select_model(layers)?;
    let models = RunModels::resolve(config, base, &next, connections)?;
    store.switch_model(&next.choice, models.main.client.base_url())?;
    agent.replace_model(
        models.main,
        approval.build(models.approval),
        models.subagents,
    );
    *selection = next;
    Ok(())
}

/// Every preset with its settings, marking the ones that give the current
/// provider, model, and effort, and the roles that use each.
fn format_presets(config: &AppConfig, base: &[ModelRequest], selection: &ModelSelection) -> String {
    let current = &selection.choice;
    let width = config
        .presets
        .keys()
        .map(String::len)
        .chain([DEFAULT_PRESET.len()])
        .max()
        .unwrap_or(0);
    let default = config.select_model(base).ok();
    let mut lines = vec![format!(
        "{} {DEFAULT_PRESET:width$}  {}",
        if default
            .as_ref()
            .is_some_and(|default| default.choice == *current)
        {
            "*"
        } else {
            " "
        },
        default
            .map(|default| default.describe())
            .unwrap_or_default(),
    )];
    for (name, preset) in &config.presets {
        let chosen = config
            .select_model(&[ModelRequest::from(current), ModelRequest::preset(name)])
            .ok();
        let marker = if chosen.is_some_and(|chosen| chosen.choice == *current) {
            "*"
        } else {
            " "
        };
        let mut line = format!("{marker} {name:width$}  {}", preset.summary());
        if let Some(description) = &preset.description {
            line.push_str(&format!("  - {description}"));
        }
        lines.push(line);
    }
    let roles: Vec<String> = config
        .agent
        .roles
        .iter()
        .filter_map(|(role, preset)| preset.map(|preset| format!("{role} = {preset}")))
        .collect();
    if !roles.is_empty() {
        lines.push(format!("roles: {}", roles.join(", ")));
    }
    lines.join("\n")
}

/// Every provider, marking the current one, with its endpoint and model.
fn format_providers(config: &AppConfig, selection: &ModelSelection) -> String {
    let current = selection.choice.provider.as_str();
    config
        .provider_names()
        .filter(|name| {
            *name == current || config.listed_provider_names().any(|listed| listed == *name)
        })
        .map(|name| {
            let marker = if name == selection.choice.provider {
                "*"
            } else {
                " "
            };
            let endpoint = config
                .provider_settings(name)
                .map(|api| endpoint(&api))
                .unwrap_or_default();
            let mut line = match config.provider_model(name) {
                Some(model) => format!("{marker} {name}  {endpoint}  (model {model})"),
                None => format!("{marker} {name}  {endpoint}"),
            };
            if config.default_provider() == name {
                line.push_str("  [default]");
            }
            if !config.provider_enabled(name) {
                line.push_str("  [disabled]");
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The models of the current provider, marking the one in use and those the
/// config disables.
fn format_models(selection: &ModelSelection, listed: Option<Vec<String>>) -> String {
    let provider = &selection.choice.provider;
    let known = KnownModels::new(listed, &selection.api.models);
    if known.names.is_empty() {
        return match known.listed {
            Some(_) => format!("provider '{provider}' lists no models"),
            None => format!("provider '{provider}' does not list its models; register them with `ano model add MODEL --provider {provider}`"),
        };
    }
    let filter = selection.api.model_filter();
    known
        .names
        .iter()
        .map(|model| {
            let marker = if *model == selection.choice.model {
                "*"
            } else {
                " "
            };
            let mut line = format!("{marker} {model}");
            if !filter.is_enabled(model) {
                line.push_str("  [disabled]");
            }
            if known.only_registered(model) {
                line.push_str("  [registered]");
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The request that `/skill` sends: turn what worked in this conversation
/// into a skill, or say that nothing is worth keeping.
fn skill_prompt(focus: &str) -> String {
    let focus = if focus.is_empty() {
        String::new()
    } else {
        format!(" The user wants it to cover: {focus}")
    };
    format!("Look back over this conversation and save the approach that worked as a skill with {SKILL_SAVE_NAME}.{focus} If a skill for the same kind of task is listed, read it with {SKILL_READ_NAME} and save the improved version under the same name instead of adding a new one. Keep what will help with similar tasks later: the steps, commands, checks, and pitfalls, not the details of this one case. If nothing in this conversation is worth saving, say so and do not save. Reply in the language the user has been using.")
}

#[cfg(test)]
mod tests {
    use super::{format_providers, switch_model};
    use crate::{
        application::{
            agent::Agent, approval::AlwaysApprove, ports::ConversationStore, registry::ToolRegistry,
        },
        config::{AppConfig, ModelRequest},
        domain::{approval::ApprovalMode, policy::UserPolicy, session::SessionBinding},
        harness::approval::ApprovalFactory,
        infrastructure::{mcp::McpPool, memory_store::MemoryConversation, openai::OpenAiClient},
    };
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn switches_provider_and_model_and_keeps_the_old_ones_on_failure() {
        let config = AppConfig::parse("[agent]\nmodel = 'gpt-main'\n[api]\nbase_url = 'http://127.0.0.1:1234/v1'\n[providers.lan]\nbase_url = 'http://192.168.1.10:1234/v1/'\nmodel = 'qwen'").unwrap();
        let mut selection = config.select_model(&[]).unwrap();
        let mut agent = Agent::new(
            OpenAiClient::new("", "http://127.0.0.1:1234/v1"),
            Default::default(),
            Arc::new(McpPool::new(Vec::new())),
            ToolRegistry::new(),
            UserPolicy::default(),
            Arc::new(AlwaysApprove),
        );
        let approval = ApprovalFactory {
            mode: ApprovalMode::Ask,
            ask_user: Arc::new(AlwaysApprove),
        };
        let binding = SessionBinding {
            endpoint: "http://127.0.0.1:1234/v1".into(),
            ..SessionBinding::new(&Default::default(), "").unwrap()
        };
        let mut store = MemoryConversation::new(binding);
        store
            .begin_turn(&json!([{"role":"user","content":"hello"}]))
            .unwrap();
        store
            .record_response(
                "r1",
                &[json!({"type":"reasoning","encrypted_content":"opaque"})],
            )
            .unwrap();
        store.complete().unwrap();
        let mut connections = Default::default();
        let mut switch = |provider: Option<&str>, model: Option<&str>| {
            let request = ModelRequest {
                provider: provider.map(str::to_string),
                model: model.map(str::to_string),
                ..ModelRequest::default()
            };
            switch_model(
                &config,
                &[ModelRequest::from(&selection.choice), request],
                &[],
                &mut connections,
                &mut agent,
                &mut store,
                &approval,
                &mut selection,
            )
        };

        switch(None, Some("gpt-other")).unwrap();
        switch(Some("lan"), None).unwrap();
        assert!(switch(Some("missing"), None).is_err());
        switch(None, Some("llama")).unwrap();

        assert_eq!(selection.choice.provider, "lan");
        assert_eq!(agent.model(), "llama");
        assert_eq!(agent.endpoint(), "http://192.168.1.10:1234/v1");
        assert_eq!(store.data().binding.endpoint, "http://192.168.1.10:1234/v1");
        assert!(store
            .data()
            .history
            .iter()
            .all(|item| item["type"] != "reasoning"));
        let listing = format_providers(&config, &selection);
        // OPENAI_BASE_URL of the test environment may redirect [api].
        assert!(listing.contains("  api  ") && listing.contains("(model gpt-main)"));
        assert!(listing.contains("* lan  http://192.168.1.10:1234/v1/  (model qwen)"));
    }
}
