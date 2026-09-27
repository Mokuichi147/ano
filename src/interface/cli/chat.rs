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
    prepare_agent, AgentOptions, PreparedAgent,
};
use crate::{
    application::{
        agent::RunRequest,
        input::InputPart,
        ports::{ApprovalHandler, ConversationStore, McpApprovalRequest},
        registry::ToolRegistry,
    },
    config::AppConfig,
    domain::session::SessionStatus,
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
  /usage   show token usage of this conversation
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
        agent,
        context,
        binding,
        session,
        mcp,
        answer,
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
                command if command.starts_with('/') => {
                    eprintln!("unknown command {command}; type /help");
                    continue;
                }
                _ => {}
            }

            let request = RunRequest {
                input: vec![InputPart::Text(prompt.to_string())],
                context: context.clone(),
            };
            let outcome = tokio::select! {
                result = agent.run_in_session(request, store.as_mut()) => Some(result),
                _ = tokio::signal::ctrl_c() => None,
            };
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
