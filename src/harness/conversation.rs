//! Conversations that a client runs turn by turn, as the web UI and editors
//! do: opened once, one turn at a time, cancellable, and ended once.

use super::{
    approval::ApprovalFactory,
    models::{Connections, RunModels},
    profile::ExecutionProfile,
    Harness,
};
use crate::{
    application::{
        agent::{Agent, AgentResult, RunRequest},
        input::InputPart,
        ports::{ConversationStore, McpGateway},
    },
    config::{AppConfig, ModelRequest, ModelSelection},
    domain::{
        session::{SessionBinding, SessionStatus},
        tool::ToolContext,
    },
    infrastructure::memory_store::MemoryConversation,
};
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, OwnedMutexGuard};

/// What a turn needs exclusively: one turn runs at a time.
pub struct Conversation {
    pub agent: Agent,
    pub store: MemoryConversation,
    pub context: ToolContext,
    /// The provider and model in use.
    pub selection: ModelSelection,
    /// Clients of the providers in use, shared when the model changes.
    pub connections: Connections,
}

/// A conversation before its agent is built: its run settings, with the
/// instructions of the workspace and the user's skills, and its models.
pub struct Opening {
    pub profile: ExecutionProfile,
    pub models: RunModels,
    pub endpoint: String,
    selection: ModelSelection,
    connections: Connections,
    binding: SessionBinding,
}

impl Opening {
    /// Add the instructions to `profile`, off the async runtime, and connect
    /// the models of `selection`; `base` is what the roles' `default` preset
    /// stands for. Skills that cannot be read are left out with a warning.
    pub async fn prepare(
        harness: &Harness,
        config: &AppConfig,
        mut profile: ExecutionProfile,
        selection: ModelSelection,
        base: &[ModelRequest],
    ) -> Result<Self> {
        let harness = harness.clone();
        let profile = tokio::task::spawn_blocking(move || {
            harness.add_instructions(&mut profile).map(|problems| {
                for problem in problems {
                    eprintln!("warning: skipped skill {problem}");
                }
                profile
            })
        })
        .await
        .context("instructions task failed")??;
        let mut connections = Connections::default();
        let models = RunModels::resolve(config, base, &selection, &mut connections)?;
        let endpoint = models.main.client.base_url().to_string();
        let binding = SessionBinding::new(&profile.context, &endpoint)?;
        Ok(Self {
            profile,
            models,
            endpoint,
            selection,
            connections,
            binding,
        })
    }

    /// Build the conversation's agent, whose calls that need approval are
    /// answered as `approval` says; `wire` adds the client's listeners.
    pub fn open(
        self,
        harness: &Harness,
        mcp: Arc<dyn McpGateway>,
        approval: &ApprovalFactory,
        wire: impl FnOnce(Agent) -> Agent,
    ) -> Conversation {
        let Self {
            profile,
            models,
            selection,
            connections,
            binding,
            ..
        } = self;
        Conversation {
            agent: wire(harness.agent(&profile, models, mcp, approval)),
            store: MemoryConversation::new(binding),
            context: profile.context,
            selection,
            connections,
        }
    }
}

/// Why a turn cannot start.
#[derive(Debug, PartialEq, Eq)]
pub enum TurnRefused {
    /// Another turn runs.
    Busy,
    /// The session ended.
    Closed,
}

impl std::fmt::Display for TurnRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Busy => "a turn is already running in this session",
            Self::Closed => "the session has ended",
        })
    }
}

impl std::error::Error for TurnRefused {}

/// A conversation with the turns that run in it.
pub struct Turns {
    conversation: Arc<tokio::sync::Mutex<Conversation>>,
    control: Mutex<TurnControl>,
}

/// Whether the session ended, and how to cancel its running turn. Kept
/// under one lock, so a turn never starts after the session ends.
#[derive(Default)]
struct TurnControl {
    closed: bool,
    cancel: Option<oneshot::Sender<()>>,
}

/// A turn that may run: it holds the conversation until it ends.
pub struct Turn {
    conversation: OwnedMutexGuard<Conversation>,
    cancelled: oneshot::Receiver<()>,
}

/// How a turn ended.
pub enum TurnOutcome {
    /// The run ended.
    Completed(Box<AgentResult>),
    /// The run failed. The failure is recorded in the conversation; the next
    /// turn continues from it.
    Failed(anyhow::Error),
    /// The turn was cancelled; the error, if recording the interruption in
    /// the conversation failed.
    Cancelled(Option<anyhow::Error>),
}

impl Turns {
    pub fn new(conversation: Conversation) -> Self {
        Self {
            conversation: Arc::new(tokio::sync::Mutex::new(conversation)),
            control: Mutex::default(),
        }
    }

    /// Take the conversation for a turn. It can be cancelled from the moment
    /// this returns, before it runs.
    pub fn start(&self) -> Result<Turn, TurnRefused> {
        let mut control = self.lock_control();
        if control.closed {
            return Err(TurnRefused::Closed);
        }
        let conversation = Arc::clone(&self.conversation)
            .try_lock_owned()
            .map_err(|_| TurnRefused::Busy)?;
        let (cancel, cancelled) = oneshot::channel();
        control.cancel = Some(cancel);
        Ok(Turn {
            conversation,
            cancelled,
        })
    }

    /// The conversation, while no turn runs.
    pub fn between_turns(&self) -> Result<tokio::sync::MutexGuard<'_, Conversation>, TurnRefused> {
        self.conversation.try_lock().map_err(|_| TurnRefused::Busy)
    }

    /// Cancel the running turn. Completed actions are not undone. False
    /// when no turn runs.
    pub fn cancel(&self) -> bool {
        self.lock_control()
            .cancel
            .take()
            .is_some_and(|cancel| cancel.send(()).is_ok())
    }

    /// Refuse further turns and cancel the running one.
    pub fn close(&self) {
        let mut control = self.lock_control();
        control.closed = true;
        if let Some(cancel) = control.cancel.take() {
            cancel.send(()).ok();
        }
    }

    /// Close, and wait until the running turn has stopped.
    pub async fn end(&self) {
        self.close();
        drop(self.conversation.lock().await);
    }

    fn lock_control(&self) -> std::sync::MutexGuard<'_, TurnControl> {
        self.control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Turn {
    /// Run the turn with `input` until it ends or is cancelled. A cancelled
    /// turn is recorded as interrupted. Returns the conversation, which stays
    /// taken until it is dropped, with how the turn ended.
    pub async fn run(
        self,
        input: Vec<InputPart>,
        approval_request: Option<String>,
    ) -> (OwnedMutexGuard<Conversation>, TurnOutcome) {
        let Self {
            mut conversation,
            cancelled,
        } = self;
        let Conversation {
            agent,
            store,
            context,
            ..
        } = &mut *conversation;
        let request = RunRequest {
            input,
            raw_input: None,
            context: context.clone(),
            goal: None,
            approval_request,
        };
        // A dropped sender cancels as well. Cancellation comes first, so a
        // turn cancelled before it ran does not start.
        let outcome = tokio::select! {
            biased;
            _ = cancelled => None,
            result = agent.run_in_session(request, store) => Some(result),
        };
        let outcome = match outcome {
            Some(Ok(result)) => TurnOutcome::Completed(Box::new(result)),
            Some(Err(error)) => TurnOutcome::Failed(error),
            None => TurnOutcome::Cancelled(
                (store.data().status == SessionStatus::Running)
                    .then(|| store.fail("Interrupted by the user."))
                    .and_then(Result::err),
            ),
        };
        (conversation, outcome)
    }
}
