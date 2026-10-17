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
    /// The problems of the skills that were left out.
    pub skipped_skills: Vec<String>,
    selection: ModelSelection,
    connections: Connections,
    binding: SessionBinding,
}

impl Opening {
    /// Add the instructions to `profile`, off the async runtime, and connect
    /// the models of `selection`; `base` is what the roles' `default` preset
    /// stands for. Skills that cannot be read are left out, in
    /// `skipped_skills`.
    pub async fn prepare(
        harness: &Harness,
        config: &AppConfig,
        mut profile: ExecutionProfile,
        selection: ModelSelection,
        base: &[ModelRequest],
    ) -> Result<Self> {
        let harness = harness.clone();
        let (profile, skipped_skills) = tokio::task::spawn_blocking(move || {
            harness
                .add_instructions(&mut profile)
                .map(|problems| (profile, problems))
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
            skipped_skills,
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
    control: Arc<Mutex<TurnControl>>,
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
    control: Arc<Mutex<TurnControl>>,
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
            control: Arc::default(),
        }
    }

    /// Take the conversation for a turn. It can be cancelled from the moment
    /// this returns, before it runs.
    pub fn start(&self) -> Result<Turn, TurnRefused> {
        let mut control = lock_control(&self.control);
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
            control: Arc::clone(&self.control),
        })
    }

    /// The conversation, while no turn runs and the session has not ended.
    pub fn between_turns(&self) -> Result<tokio::sync::MutexGuard<'_, Conversation>, TurnRefused> {
        if lock_control(&self.control).closed {
            return Err(TurnRefused::Closed);
        }
        self.conversation.try_lock().map_err(|_| TurnRefused::Busy)
    }

    /// Cancel the running turn. Completed actions are not undone. False
    /// when no turn runs.
    pub fn cancel(&self) -> bool {
        lock_control(&self.control)
            .cancel
            .take()
            .is_some_and(|cancel| cancel.send(()).is_ok())
    }

    /// Refuse further turns and cancel the running one.
    pub fn close(&self) {
        let mut control = lock_control(&self.control);
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
}

fn lock_control(control: &Mutex<TurnControl>) -> std::sync::MutexGuard<'_, TurnControl> {
    control
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            control,
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
        // Nothing runs to cancel now; the next turn cannot start before the
        // conversation is returned.
        lock_control(&control).cancel = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::{approval::AlwaysApprove, registry::ToolRegistry},
        domain::policy::UserPolicy,
        infrastructure::{mcp::McpPool, openai::OpenAiClient},
    };
    use serde_json::json;

    /// A conversation whose model is never reached: the turns of these
    /// tests are cancelled before they run.
    fn turns() -> Turns {
        let config = AppConfig::parse("[api]\nbase_url = 'http://127.0.0.1:9/v1'").unwrap();
        let agent = Agent::new(
            OpenAiClient::new("", "http://127.0.0.1:9/v1"),
            Default::default(),
            Arc::new(McpPool::new(Vec::new())),
            ToolRegistry::new(),
            UserPolicy::default(),
            Arc::new(AlwaysApprove),
        );
        let context = ToolContext::default();
        let binding = SessionBinding::new(&context, "http://127.0.0.1:9/v1").unwrap();
        Turns::new(Conversation {
            agent,
            store: MemoryConversation::new(binding),
            context,
            selection: config.select_model(&[]).unwrap(),
            connections: Connections::default(),
        })
    }

    #[tokio::test]
    async fn one_turn_runs_at_a_time_and_cancellation_ends_with_the_turn() {
        let turns = turns();
        assert!(!turns.cancel());
        let turn = turns.start().unwrap();
        assert_eq!(turns.start().err(), Some(TurnRefused::Busy));
        assert_eq!(turns.between_turns().err(), Some(TurnRefused::Busy));

        // Cancelled before it ran, the turn does not start.
        assert!(turns.cancel());
        let (conversation, outcome) = turn.run(Vec::new(), None).await;
        assert!(matches!(outcome, TurnOutcome::Cancelled(None)));
        assert_eq!(conversation.store.data().status, SessionStatus::Ready);
        drop(conversation);

        assert!(!turns.cancel());
        assert!(turns.between_turns().is_ok());
    }

    #[tokio::test]
    async fn a_turn_interrupted_while_running_is_recorded_as_failed() {
        let turns = turns();
        turns
            .between_turns()
            .unwrap()
            .store
            .begin_turn(&json!([{"role": "user", "content": "hello"}]))
            .unwrap();
        let turn = turns.start().unwrap();
        turns.cancel();
        let (conversation, outcome) = turn.run(Vec::new(), None).await;
        assert!(matches!(outcome, TurnOutcome::Cancelled(None)));
        assert_eq!(conversation.store.data().status, SessionStatus::Failed);
    }

    #[tokio::test]
    async fn an_ended_session_refuses_turns_and_changes() {
        let turns = turns();
        turns.end().await;
        assert_eq!(turns.start().err(), Some(TurnRefused::Closed));
        assert_eq!(turns.between_turns().err(), Some(TurnRefused::Closed));
    }
}
