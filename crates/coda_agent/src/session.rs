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
use crate::persist::{StoredCheckpoint, StoredResumePoint, StoredRuntimeSnapshot};
use crate::runtime::{AgentRuntime, AgentRuntimeSnapshot, SendCommandError, SessionStorage};
use crate::{
    AgentEvent, AgentTeam, Envelope, PendingApproval, ResumeDecision, RunConfig, Sender, ThreadId,
    ToolCallResolution,
};
use coda_core::llm::{LLMProvider, Message};
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

/// What to do when a graceful shutdown hits its timeout.
#[derive(Debug, Clone, Copy)]
pub enum OnTimeout {
    /// Return `false` from `shutdown` and leave agents running.
    Return,
    /// Abort all in-flight work and wait (unbounded) for full shutdown.
    Abort,
}

/// Shutdown strategy for [`Session::shutdown`].
#[derive(Debug, Clone, Copy)]
pub enum Shutdown {
    Graceful {
        /// `None` waits unbounded for agents to exit (`on_timeout` never fires).
        timeout: Option<Duration>,
        on_timeout: OnTimeout,
    },
    Abort,
}

impl Shutdown {
    pub fn graceful(timeout: Duration) -> Self {
        Shutdown::Graceful {
            timeout: Some(timeout),
            on_timeout: OnTimeout::Return,
        }
    }

    pub fn graceful_then_abort(timeout: Duration) -> Self {
        Shutdown::Graceful {
            timeout: Some(timeout),
            on_timeout: OnTimeout::Abort,
        }
    }

    /// Wait unbounded for in-flight work to reach its next checkpoint and the
    /// agents to exit; never aborts. `shutdown` returning `true` is then a
    /// durability barrier: every agent's final checkpoint is on disk.
    pub fn graceful_unbounded() -> Self {
        Shutdown::Graceful {
            timeout: None,
            on_timeout: OnTimeout::Return,
        }
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
}

impl<P: LLMProvider + Clone> Default for SessionBuilder<'_, P> {
    fn default() -> Self {
        Self {
            storage: None,
            team: None,
            run_config: None,
            session_id: None,
            resume_decisions: HashMap::new(),
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

        let (team, workspace_dir) = self.team.take().ok_or(OpenError::MissingField("team"))?;
        let agents = team.build(&workspace_dir);
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

        let resumed_messages: Option<Vec<Message>> = storage
            .load_checkpoint(&session_id)
            .await
            .map_err(OpenError::Storage)?
            .map(|ckpt| ckpt.messages);

        let pending_approvals =
            collect_pending_approvals(storage.as_ref(), &session_id, &root_name, snapshot.as_ref())
                .await?;

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
                    resume_decisions.insert(p.thread_id.clone(), ResumeDecision { resolutions });
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
        // Drop decisions that don't match any pending approval so stale entries
        // can't mask coverage bugs by silently disappearing into bootstrap.
        resume_decisions.retain(|tid, _| pending_approvals.iter().any(|c| &c.thread_id == tid));

        let has_resuming_agents = snapshot.as_ref().is_some_and(|snapshot| {
            !snapshot.active_threads.is_empty()
                || snapshot
                    .agent_drained_envelopes
                    .values()
                    .any(|v| !v.is_empty())
                || snapshot.drained_envelopes.values().any(|v| !v.is_empty())
        });

        let mut runtime = AgentRuntime::new(storage, session_id.clone());
        // CRITICAL: subscribe before bootstrap so no events are lost between
        // spawn and the caller's first `recv`.
        let events_rx = runtime.subscribe();
        runtime
            .bootstrap(agents, snapshot, resume_decisions, run_config)
            .await;

        Ok(Session {
            inner: Arc::new(SessionInner {
                runtime,
                root_name,
                session_id,
                resumed_messages,
                has_resuming_agents,
                events_rx: Mutex::new(events_rx),
            }),
        })
    }
}

/// Walks root + snapshot-tracked agent threads, loads each checkpoint, and
/// returns those still sitting in `PendingApproval`.
async fn collect_pending_approvals(
    storage: &dyn SessionStorage,
    session_id: &str,
    root_name: &str,
    snapshot: Option<&AgentRuntimeSnapshot>,
) -> Result<Vec<PendingApproval>, OpenError> {
    let mut seen_thread_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut threads: Vec<String> = Vec::new();
    // Root always has thread_id == session_id.
    threads.push(session_id.to_string());
    seen_thread_ids.insert(session_id.to_string());
    if let Some(snap) = snapshot {
        for (agent_name, tid) in &snap.active_threads {
            if agent_name == root_name {
                continue;
            }
            if seen_thread_ids.insert(tid.clone()) {
                threads.push(tid.clone());
            }
        }
    }

    let mut pending = Vec::new();
    for tid in threads {
        let stored: Option<StoredCheckpoint> = storage
            .load_checkpoint(&tid)
            .await
            .map_err(OpenError::Storage)?;
        if let Some(stored) = stored
            && let StoredResumePoint::PendingApproval {
                ref pending_approval_calls,
                ..
            } = stored.resume_point
            && !pending_approval_calls.is_empty()
        {
            pending.push(PendingApproval {
                thread_id: stored.thread_id,
                agent_name: stored.agent_name,
                calls: pending_approval_calls.clone(),
                suspended_at: stored.suspended_at,
            });
        }
    }
    Ok(pending)
}

struct SessionInner {
    runtime: AgentRuntime,
    root_name: String,
    session_id: String,
    resumed_messages: Option<Vec<Message>>,
    has_resuming_agents: bool,
    events_rx: Mutex<broadcast::Receiver<(String, ThreadId, AgentEvent)>>,
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

    /// `true` when the snapshot indicates that at least one agent has in-flight
    /// work (active thread, or queued envelopes) and will therefore emit events
    /// immediately after `open` — without waiting for a `send`. Callers should
    /// enter the event loop directly instead of prompting for user input first.
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
    /// `images` is a list of base64 data-URIs (`data:image/<fmt>;base64,<b64>`)
    /// or HTTPS URLs. Pass an empty `Vec` for text-only turns.
    pub async fn send(
        &self,
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
                body: EnvelopeBody::Task { task, images },
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

    /// Stop the session. Returns `true` when all agents exited within the
    /// requested policy (or immediately, for `Shutdown::Abort`).
    pub async fn shutdown(&self, mode: Shutdown) -> bool {
        match mode {
            Shutdown::Graceful {
                timeout,
                on_timeout,
            } => {
                self.inner.runtime.request_exit().await;
                let ok = self.inner.runtime.wait_for_exit(timeout).await;
                if !ok {
                    match on_timeout {
                        OnTimeout::Return => false,
                        OnTimeout::Abort => {
                            self.inner.runtime.request_abort().await;
                            self.inner.runtime.wait_for_exit(None).await
                        }
                    }
                } else {
                    true
                }
            }
            Shutdown::Abort => {
                self.inner.runtime.request_abort().await;
                self.inner.runtime.request_exit().await;
                self.inner.runtime.wait_for_exit(None).await
            }
        }
    }

    fn wrap_event(&self, (name, thread_id, kind): (String, ThreadId, AgentEvent)) -> SessionEvent {
        let origin = if name == self.inner.root_name {
            EventOrigin::Root
        } else {
            EventOrigin::Sub { name }
        };
        SessionEvent {
            origin,
            thread_id,
            kind,
        }
    }
}
