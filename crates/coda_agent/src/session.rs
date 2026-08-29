//! High-level session facade over [`AgentRuntime`].
//!
//! `Session` wraps an `AgentRuntime` and exposes a small API tailored for the
//! common case: one root agent with some subagents, send a task, consume
//! events, resume when suspended, shut down cleanly. Both sync and async HITL
//! flows use the same surface — the only difference lives in the caller's
//! `Suspended` handler.
//!
//! Callers that need finer control can reach the underlying runtime through
//! [`Session::runtime`].

use crate::agent::{EnvelopeBody, Receiver};
use crate::persist::{StoredResumePoint, StoredRuntimeSnapshot};
use crate::runtime::{
    AgentRuntime, AgentRuntimeSnapshot, ResumeTarget, SendCommandError, SessionStorage,
};
use crate::{
    AgentEvent, AgentTeam, Envelope, PendingApproval, ResumeDecision, RunConfig, Sender, ThreadId,
    ToolCallResolution,
};
use coda_background::BackgroundProcesses;
use coda_core::llm::{LLMProvider, Message, MessageId, TurnId};
use coda_tools::KeyedLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, broadcast};
use tracing::warn;
use uuid::Uuid;

/// Origin of a [`SessionEvent`]: the root agent, or a named subagent.
///
/// `thread_id` on the event still disambiguates stateless subagent instances.
#[derive(Debug, Clone)]
pub enum EventOrigin {
    Root,
    Sub { name: String },
}

impl EventOrigin {
    pub fn is_root(&self) -> bool {
        matches!(self, EventOrigin::Root)
    }

    pub fn subagent_name(&self) -> Option<&str> {
        match self {
            EventOrigin::Root => None,
            EventOrigin::Sub { name } => Some(name.as_str()),
        }
    }
}

/// An event produced by a [`Session`], with the raw [`AgentEvent`] and the
/// origin agent distinguished.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub origin: EventOrigin,
    pub thread_id: ThreadId,
    /// The submission this event belongs to. Shared by every agent the turn
    /// reaches, so a consumer can settle per turn without working out the call
    /// tree for itself.
    pub turn_id: TurnId,
    pub kind: AgentEvent,
}

/// An item yielded by [`Session::recv`]. `Lagged` surfaces broadcast overflow
/// to the caller instead of silently dropping events: consumers that
/// reconstruct state from the stream must know their view has a gap.
// Not boxed: items are consumed immediately, so the size imbalance never sits
// in a collection, and boxing would cost an allocation per event.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum SessionStreamItem {
    Event(SessionEvent),
    /// The receiver fell behind and `n` events were dropped.
    Lagged(u64),
}

/// How long a cancelled runtime is given to stop on its own before its tasks
/// are dropped where they stand.
///
/// Cancellation is a request, and a task parked inside a write or a stream
/// never reads it. Past this point the only thing left that ends such a task is
/// dropping it — and something has to, because callers reopen the session the
/// moment shutdown returns.
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

/// Shutdown strategy for [`Session::shutdown`].
#[derive(Debug, Clone, Copy)]
pub enum Shutdown {
    /// Ask the agents to exit, giving them `timeout` to finish what they are
    /// doing before they are cancelled and, failing that, dropped.
    Graceful { timeout: Option<Duration> },
    /// Cancel in-flight work up front rather than letting it finish.
    Abort,
}

impl Shutdown {
    pub fn graceful_then_abort(timeout: Duration) -> Self {
        Shutdown::Graceful {
            timeout: Some(timeout),
        }
    }

    /// Wait unbounded for in-flight work to reach its next checkpoint and the
    /// agents to exit; never cancels anything. `shutdown` returning `true` is
    /// then a durability barrier: every agent's final checkpoint is on disk.
    ///
    /// The price of never cutting a turn short is that this alone among the
    /// modes can fail to return at all, so it is only for callers that know
    /// nothing is wedged — a session already judged idle, say. A caller
    /// reacting to something being wrong wants a deadline.
    pub fn graceful_unbounded() -> Self {
        Shutdown::Graceful { timeout: None }
    }

    pub fn abort() -> Self {
        Shutdown::Abort
    }
}

/// Errors produced by [`SessionBuilder::open`].
#[derive(Debug)]
pub enum OpenError {
    MissingField(&'static str),
    Storage(String),
    /// One or more agents have a checkpoint in `PendingApproval` state but the
    /// builder's `resume_decisions` did not cover them. The runtime is NOT
    /// started in this case; the caller should collect resume decisions for the
    /// returned pending approvals (keyed by `thread_id`) and rebuild the session
    /// with `SessionBuilder::resume_decisions`.
    PendingApprovalsRequired(Vec<PendingApproval>),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::MissingField(name) => write!(f, "missing required field '{name}'"),
            OpenError::Storage(err) => write!(f, "storage error: {err}"),
            OpenError::PendingApprovalsRequired(ckpts) => {
                write!(
                    f,
                    "session has {} pending approval(s) without resume decisions",
                    ckpts.len()
                )
            }
        }
    }
}

impl std::error::Error for OpenError {}

/// Hand-written builder for [`Session`]. Borrows the [`AgentTeam`] (`'a`) until
/// [`open`](SessionBuilder::open), which builds it into the session's agents.
pub struct SessionBuilder<'a, P: LLMProvider + Clone> {
    storage: Option<Arc<dyn SessionStorage>>,
    team: Option<(&'a AgentTeam, String)>,
    run_config: Option<RunConfig<P>>,
    session_id: Option<String>,
    resume_decisions: HashMap<String, ResumeDecision>,
    file_locks: Option<Arc<KeyedLock<String>>>,
    background: Option<Arc<BackgroundProcesses>>,
}

impl<P: LLMProvider + Clone> Default for SessionBuilder<'_, P> {
    fn default() -> Self {
        Self {
            storage: None,
            team: None,
            run_config: None,
            session_id: None,
            resume_decisions: HashMap::new(),
            file_locks: None,
            background: None,
        }
    }
}

impl<'a, P: LLMProvider + Clone + 'static> SessionBuilder<'a, P> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn storage<S: SessionStorage + 'static>(mut self, storage: S) -> Self {
        self.storage = Some(Arc::new(storage));
        self
    }

    /// Register the validated [`AgentTeam`] to run, and the workspace its tools
    /// build against. The team is borrowed and built into fresh agents at
    /// [`open`](SessionBuilder::open); the team carries its own root, so there is
    /// no root name to pass and no way to name a root that isn't present.
    pub fn team(mut self, team: &'a AgentTeam, workspace_dir: &str) -> Self {
        self.team = Some((team, workspace_dir.to_string()));
        self
    }

    pub fn run_config(mut self, config: RunConfig<P>) -> Self {
        self.run_config = Some(config);
        self
    }

    /// Registry the file tools serialize writes on. Defaults to the
    /// process-wide [`coda_tools::shared_file_locks`], which is what keeps two
    /// sessions over one workspace from clobbering each other's edits. Override
    /// only to isolate sessions on purpose — tests, mainly.
    pub fn file_locks(mut self, locks: Arc<KeyedLock<String>>) -> Self {
        self.file_locks = Some(locks);
        self
    }

    /// Inject an externally-owned background task registry (e.g. held by the
    /// server's session hub, so tasks survive session rebuilds like a model
    /// switch). The session then never shuts the registry down — its owner
    /// does. Without this call the session builds a private registry and
    /// [`Session::shutdown`] tears it down once the runtime has exited.
    pub fn background(mut self, registry: Arc<BackgroundProcesses>) -> Self {
        self.background = Some(registry);
        self
    }

    /// If unset, a fresh UUID is generated. Provide an existing id to resume a
    /// prior session (the snapshot + root checkpoint are loaded automatically).
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    /// Provide resume decisions for any agents whose restored checkpoint is in
    /// `PendingApproval` state. Keys are `PendingApproval::thread_id` values
    /// (use those returned by [`OpenError::PendingApprovalsRequired`]).
    ///
    /// If `open` finds pending-approval checkpoints that are not covered by
    /// this map, it fails with [`OpenError::PendingApprovalsRequired`] and
    /// the agent runtime is NOT started.
    pub fn resume_decisions(mut self, decisions: HashMap<String, ResumeDecision>) -> Self {
        self.resume_decisions = decisions;
        self
    }

    pub async fn open(mut self) -> Result<Session, OpenError> {
        let storage = self
            .storage
            .take()
            .ok_or(OpenError::MissingField("storage"))?;
        let run_config = self
            .run_config
            .take()
            .ok_or(OpenError::MissingField("run_config"))?;

        // An injected registry is externally owned (the server's session hub
        // holds it, so tasks outlive session rebuilds like a model switch) and
        // this session only borrows it. A self-built one is owned, and
        // `shutdown` tears it down. Resolved before the agents are built —
        // their tools capture it.
        let (background, owns_background) = match self.background.take() {
            Some(registry) => (registry, false),
            None => (Arc::new(BackgroundProcesses::temporary()), true),
        };

        let (team, workspace_dir) = self.team.take().ok_or(OpenError::MissingField("team"))?;
        let file_locks = self
            .file_locks
            .take()
            .unwrap_or_else(coda_tools::shared_file_locks);
        let agents = team.build(&workspace_dir, file_locks, background.clone());
        let root_name = team.root().name.to_string();

        let session_id = self
            .session_id
            .take()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        // Load resumed state BEFORE bootstrap so we can (a) surface root history
        // via `resumed_messages` and (b) detect pending approvals on *any*
        // agent in the snapshot, not just the root.
        let stored_snapshot: Option<StoredRuntimeSnapshot> = storage
            .load_session_snapshot(&session_id)
            .await
            .map_err(OpenError::Storage)?;
        let snapshot: Option<AgentRuntimeSnapshot> = stored_snapshot.map(Into::into);

        // Turn tags stay server-side: callers get the conversation, not the
        // control-flow metadata around it.
        let resumed_messages: Option<Vec<Message>> = storage
            .load_checkpoint(&session_id)
            .await
            .map_err(OpenError::Storage)?
            .map(|ckpt| {
                ckpt.messages
                    .into_iter()
                    .map(|entry| entry.message)
                    .collect()
            });

        let mut pending_approvals =
            collect_pending_approvals(storage.as_ref(), &session_id).await?;
        pending_approvals.retain(|approval| {
            let available = agents.contains_key(&approval.agent_name);
            if !available {
                warn!(
                    "ignoring pending approval on thread {} for unavailable agent {}",
                    approval.thread_id, approval.agent_name
                );
            }
            available
        });

        let mut resume_decisions = self.resume_decisions;

        // Auto-reject timed-out pending approvals that the caller didn't cover.
        if let Some(timeout) = run_config.approval_timeout {
            for p in &pending_approvals {
                if resume_decisions.contains_key(&p.thread_id) {
                    continue;
                }
                let elapsed_ms = (jiff::Timestamp::now().as_millisecond()
                    - p.suspended_at.as_millisecond())
                .max(0) as u128;
                if elapsed_ms > timeout.as_millis() {
                    let resolutions = p
                        .calls
                        .iter()
                        .map(|c| {
                            (
                                c.id.clone(),
                                ToolCallResolution::Rejected {
                                    reason: Some("approval timed out".into()),
                                },
                            )
                        })
                        .collect();
                    resume_decisions.insert(
                        p.thread_id.clone(),
                        ResumeDecision {
                            parent_message_id: p.parent_message_id,
                            resolutions,
                        },
                    );
                }
            }
        }

        let uncovered: Vec<PendingApproval> = pending_approvals
            .iter()
            .filter(|c| !resume_decisions.contains_key(&c.thread_id))
            .cloned()
            .collect();
        if !uncovered.is_empty() {
            return Err(OpenError::PendingApprovalsRequired(uncovered));
        }
        // Address each decision to the agent whose checkpoint is parked on that
        // thread. This — not the runtime snapshot — is what makes a resume
        // reach its thread: the snapshot is only written when an agent exits,
        // so it is absent for a session that was killed mid-approval and for
        // one a fork just minted. Decisions that match no pending approval are
        // dropped here rather than disappearing silently into bootstrap.
        let mut resume_targets: HashMap<String, ResumeTarget> = HashMap::new();
        for approval in &pending_approvals {
            let Some(decision) = resume_decisions.remove(&approval.thread_id) else {
                continue;
            };
            if resume_targets
                .insert(
                    approval.agent_name.clone(),
                    ResumeTarget {
                        thread_id: ThreadId::from(approval.thread_id.clone()),
                        decision,
                    },
                )
                .is_some()
            {
                // One agent task drives one thread at a time, so a second
                // suspended thread for the same agent cannot be resumed in this
                // run; it stays parked and is offered again on the next open.
                warn!(
                    "agent {} has more than one suspended thread; resuming only {}",
                    approval.agent_name, approval.thread_id
                );
            }
        }
        for thread_id in resume_decisions.keys() {
            warn!("discarding a resume decision for unsuspended thread {thread_id}");
        }

        let mut runtime = AgentRuntime::new(storage, session_id.clone());
        // CRITICAL: subscribe before bootstrap so no events are lost between
        // spawn and the caller's first `recv`.
        let events_rx = runtime.subscribe();
        // Answered by bootstrap, not the snapshot: it drops what will never run.
        let has_resuming_agents = runtime
            .bootstrap(agents, snapshot, resume_targets, run_config)
            .await
            .map_err(OpenError::Storage)?;

        Ok(Session {
            inner: Arc::new(SessionInner {
                runtime,
                root_name,
                session_id,
                resumed_messages,
                has_resuming_agents,
                events_rx: Mutex::new(events_rx),
                background,
                owns_background,
            }),
        })
    }
}

/// Loads every checkpoint still sitting in `PendingApproval`.
async fn collect_pending_approvals(
    storage: &dyn SessionStorage,
    session_id: &str,
) -> Result<Vec<PendingApproval>, OpenError> {
    let mut pending = Vec::new();
    for stored in storage
        .load_pending_approval_checkpoints(session_id)
        .await
        .map_err(OpenError::Storage)?
    {
        let StoredResumePoint::PendingApproval {
            parent_message_id,
            pending_approval_calls,
            ..
        } = stored.resume_point
        else {
            return Err(OpenError::Storage(format!(
                "storage returned non-pending checkpoint {} as awaiting approval",
                stored.thread_id
            )));
        };
        if pending_approval_calls.is_empty() {
            return Err(OpenError::Storage(format!(
                "storage returned empty approval checkpoint {} as awaiting approval",
                stored.thread_id
            )));
        }
        pending.push(PendingApproval {
            thread_id: stored.thread_id,
            agent_name: stored.agent_name,
            parent_message_id,
            calls: pending_approval_calls
                .into_iter()
                .map(|prepared| prepared.tool_call)
                .collect(),
            suspended_at: stored.suspended_at,
        });
    }
    Ok(pending)
}

struct SessionInner {
    runtime: AgentRuntime,
    root_name: String,
    session_id: String,
    resumed_messages: Option<Vec<Message>>,
    has_resuming_agents: bool,
    events_rx: Mutex<broadcast::Receiver<(String, ThreadId, TurnId, AgentEvent)>>,
    background: Arc<BackgroundProcesses>,
    /// Self-built registry (no [`SessionBuilder::background`]): `shutdown`
    /// tears it down once the runtime has confirmedly exited. An injected
    /// registry is never touched — its external owner manages its lifecycle.
    owns_background: bool,
}

/// High-level handle to a running agent session.
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

impl Session {
    pub fn builder<'a, P: LLMProvider + Clone + 'static>() -> SessionBuilder<'a, P> {
        SessionBuilder::new()
    }

    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    pub fn root_name(&self) -> &str {
        &self.inner.root_name
    }

    /// `true` when at least one agent picked up in-flight work at `open` (an
    /// active thread, or a replayed envelope) and will therefore emit events
    /// without waiting for a `send`. Callers should enter the event loop
    /// directly instead of prompting for user input first.
    ///
    /// What recovery really resumed, not what the snapshot held: a turn that
    /// ended in another process is thrown out first.
    pub fn has_resuming_agents(&self) -> bool {
        self.inner.has_resuming_agents
    }

    /// The root agent's conversation history at open time, if one was on disk.
    /// Intended for callers that want to render prior conversation history
    /// (e.g. an interactive CLI).
    pub fn resumed_messages(&self) -> Option<&[Message]> {
        self.inner.resumed_messages.as_deref()
    }

    /// Send a user task to the root agent, optionally with image attachments.
    ///
    /// `message_id` becomes the identity of the user message this task turns
    /// into. The caller supplies it because it also needs to label its own copy
    /// of that message (the live snapshot) and answer the client with it.
    /// Returns [`SendCommandError::TurnAlreadyActive`] until the previous turn
    /// has reached its final, durable ending; suspension does not release it.
    ///
    /// `images` is a list of base64 data-URIs (`data:image/<fmt>;base64,<b64>`)
    /// or HTTPS URLs. Pass an empty `Vec` for text-only turns.
    pub async fn send(
        &self,
        message_id: MessageId,
        task: impl Into<String>,
        images: Vec<String>,
    ) -> Result<(), SendCommandError> {
        let task = task.into();
        let thread_id = ThreadId::from(self.inner.session_id.clone());
        let root_name = self.inner.root_name.clone();
        self.inner
            .runtime
            .send_message(Envelope::with_id(|id| Envelope {
                id,
                from: Sender::User,
                to: Receiver {
                    name: root_name,
                    thread_id,
                },
                reply_to: None,
                body: EnvelopeBody::Task {
                    message_id,
                    task,
                    images,
                },
            }))
            .await
    }

    /// Resume a suspended agent by `agent_name` and `thread_id`.
    ///
    /// The caller gets `agent_name` and `thread_id` from a
    /// [`PendingApproval`] (received via [`AgentEvent::Suspended`] or
    /// [`OpenError::PendingApprovalsRequired`]).
    pub async fn resume(
        &self,
        agent_name: &str,
        thread_id: &str,
        decision: ResumeDecision,
    ) -> Result<(), SendCommandError> {
        self.send_resume_envelope(agent_name, thread_id, decision)
            .await
    }

    async fn send_resume_envelope(
        &self,
        agent_name: &str,
        thread_id: &str,
        decision: ResumeDecision,
    ) -> Result<(), SendCommandError> {
        self.inner
            .runtime
            .send_message(Envelope::with_id(|id| Envelope {
                id,
                from: Sender::User,
                to: Receiver {
                    name: agent_name.to_string(),
                    thread_id: ThreadId::from(thread_id.to_string()),
                },
                reply_to: None,
                body: EnvelopeBody::Resume(decision),
            }))
            .await
    }

    /// Forcefully cancel whatever every agent is currently doing. Does not
    /// exit the runtime — subsequent `send`s will start fresh runs.
    pub async fn abort(&self) {
        self.inner.runtime.request_abort().await;
    }

    /// Receive the next session stream item. `None` once the runtime has shut
    /// down and all events have been drained.
    ///
    /// A lagged receiver yields [`SessionStreamItem::Lagged`] instead of
    /// silently skipping the dropped events — the caller decides how to
    /// recover (e.g. resync from the persisted checkpoint).
    pub async fn recv(&self) -> Option<SessionStreamItem> {
        let mut rx = self.inner.events_rx.lock().await;
        match rx.recv().await {
            Ok(raw) => Some(SessionStreamItem::Event(self.wrap_event(raw))),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("session event stream lagged by {n} events; dropped");
                Some(SessionStreamItem::Lagged(n))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    /// Stop the session. Returns whether the agents stopped of their own accord
    /// within the requested policy; every mode but `graceful_unbounded` also
    /// guarantees that none of them is still running once this returns.
    /// The session's background task registry (injected or self-built).
    pub fn background(&self) -> &Arc<BackgroundProcesses> {
        &self.inner.background
    }

    pub async fn shutdown(&self, mode: Shutdown) -> bool {
        let exited = self.stop_runtime(mode).await;
        // Tear down an owned registry only once the runtime has confirmedly
        // exited: a graceful timeout that returns `false` leaves the session
        // running, and killing its background tasks then would leave a
        // half-closed state (session up, registry closed). Undelivered
        // notices are dropped — a standalone session has no reopen to deliver
        // them to.
        if exited && self.inner.owns_background {
            let _ = self.inner.background.shutdown().await;
        }
        exited
    }

    async fn stop_runtime(&self, mode: Shutdown) -> bool {
        match mode {
            Shutdown::Graceful { timeout } => {
                self.inner.runtime.request_exit().await;
                let Some(duration) = timeout else {
                    return self.inner.runtime.wait_for_exit(None).await;
                };
                if self.inner.runtime.wait_for_settle(duration).await {
                    return true;
                }
                // Exiting is something an agent does between pieces of work, so
                // one that missed the deadline is inside a piece. Cancelling
                // reaches the ones that are watching for it; the wait after is
                // what ends the rest.
                self.inner.runtime.cancel_in_flight().await;
                self.inner
                    .runtime
                    .wait_for_exit(Some(TERMINATION_GRACE))
                    .await;
                false
            }
            Shutdown::Abort => {
                self.inner.runtime.cancel_in_flight().await;
                self.inner.runtime.request_exit().await;
                self.inner
                    .runtime
                    .wait_for_exit(Some(TERMINATION_GRACE))
                    .await
            }
        }
    }

    fn wrap_event(
        &self,
        (name, thread_id, turn_id, kind): (String, ThreadId, TurnId, AgentEvent),
    ) -> SessionEvent {
        let origin = if name == self.inner.root_name {
            EventOrigin::Root
        } else {
            EventOrigin::Sub { name }
        };
        SessionEvent {
            origin,
            thread_id,
            turn_id,
            kind,
        }
    }
}
