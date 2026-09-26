//! `ano chat`: a multi-turn conversation on the terminal.
//!
//! One thread reads stdin and hands out lines, so the prompt and MCP approval
//! questions never compete for input. Ctrl+C cancels the running turn and
//! keeps the conversation; `/exit` or end of input quits.

use super::{approval_for, output, prepare_agent, AgentOptions, PreparedAgent};
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
use std::{
    io::{BufRead, IsTerminal, Write},
    sync::Arc,
};
use tokio::sync::{mpsc, Mutex};

const HELP: &str = "Commands:
  /plan    show the task plan
  /usage   show token usage of this conversation
  /help    show this help
  /exit    quit (also Ctrl+D)
Ctrl+C cancels the running turn and keeps the conversation.";

/// Lines from stdin, read by a dedicated thread.
struct LineReader {
    lines: Mutex<mpsc::UnboundedReceiver<String>>,
}

impl LineReader {
    fn spawn() -> Arc<Self> {
        let (sender, lines) = mpsc::unbounded_channel();
        // A plain thread, not a runtime blocking task: it may stay blocked on
        // stdin at exit without holding up runtime shutdown.
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Arc::new(Self {
            lines: Mutex::new(lines),
        })
    }

    /// The next line, or `None` at end of input.
    async fn next_line(&self) -> Option<String> {
        self.lines.lock().await.recv().await
    }
}

/// Asks on the terminal and reads the answer through the shared reader.
struct ChatApproval {
    lines: Arc<LineReader>,
}

#[async_trait]
impl ApprovalHandler for ChatApproval {
    async fn approve(&self, request: McpApprovalRequest) -> Result<bool> {
        eprint!(
            "\nMCP approval requested: {}:{}\nArguments: {}\nAllow this call? [y/N] ",
            request.server_label, request.tool_name, request.arguments
        );
        std::io::stderr().flush().ok();
        Ok(self.lines.next_line().await.is_some_and(|answer| {
            matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        }))
    }
}

pub(super) async fn run(
    config: AppConfig,
    user_id: String,
    options: AgentOptions,
    registry: ToolRegistry,
) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    let lines = LineReader::spawn();
    let approval = approval_for(
        &config,
        &options,
        interactive,
        Arc::new(ChatApproval {
            lines: Arc::clone(&lines),
        }),
    )?;
    let PreparedAgent {
        agent,
        context,
        binding,
        session,
        mcp,
    } = prepare_agent(&config, &user_id, &options, registry, approval)?;
    let mut store: Box<dyn ConversationStore> = match session {
        Some(session) => Box::new(session),
        None => Box::new(MemoryConversation::new(binding)),
    };
    if interactive {
        eprintln!("ano chat — type /help for commands, /exit or Ctrl+D to quit.");
    }

    let result = async {
        loop {
            if interactive {
                eprint!("> ");
                std::io::stderr().flush().ok();
            }
            let line = tokio::select! {
                line = lines.next_line() => line,
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("\n(use /exit or Ctrl+D to quit)");
                    continue;
                }
            };
            let Some(line) = line else { break };
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
                Some(Ok(result)) => println!("{}\n", output::format_result(&result, false)?),
                // The failure is recorded in the conversation; the next turn
                // can continue from it.
                Some(Err(error)) => eprintln!("error: {error:#}"),
                None => {
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
