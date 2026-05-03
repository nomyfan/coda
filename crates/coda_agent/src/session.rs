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

use crate::agent::{EnvelopeBody, Receiver, ResumePoint};
use crate::runtime::{AgentRuntime, AgentRuntimeSnapshot, SendCommandError, SessionStorage};
use crate::{
    Agent, AgentCheckpoint, AgentEvent, AgentSpec, BuildContext, BuildError, Envelope,
    PendingApproval, ResumeDecision, RunConfig, Sender, ThreadId, ToolCallResolution,
};
use coda_core::llm::LLMProvider;
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
        timeout: Duration,
        on_timeout: OnTimeout,
    },
    Abort,
}

impl Shutdown {
    pub fn graceful(timeout: Duration) -> Self {
        Shutdown::Graceful {
            timeout,
            on_timeout: OnTimeout::Return,
        }
    }

    pub fn graceful_then_abort(timeout: Duration) -> Self {
        Shutdown::Graceful {
            timeout,
            on_timeout: OnTimeout::Abort,
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
    Build(BuildError),
    Storage(String),
    UnknownRoot(String),
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
            OpenError::Build(err) => write!(f, "failed to build agents: {err}"),
            OpenError::Storage(err) => write!(f, "storage error: {err}"),
            OpenError::UnknownRoot(name) => {
                write!(f, "root '{name}' not present in provided agents")
            }
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

enum AgentSource {
    Spec {
        spec: AgentSpec,
        ctx: BuildContext,
    },
    PreBuilt {
        agents: HashMap<String, Agent>,
        root_name: String,
    },
}

/// Hand-written builder for [`Session`].
pub struct SessionBuilder<P: LLMProvider + Clone> {
    storage: Option<Arc<dyn SessionStorage>>,
    source: Option<AgentSource>,
    build_context: Option<BuildContext>,
    pending_spec: Option<AgentSpec>,
    run_config: Option<RunConfig<P>>,
    session_id: Option<String>,
    resume_decisions: HashMap<String, ResumeDecision>,
}

impl<P: LLMProvider + Clone> Default for SessionBuilder<P> {
    fn default() -> Self {
        Self {
            storage: None,
            source: None,
            build_context: None,
            pending_spec: None,
            run_config: None,
            session_id: None,
            resume_decisions: HashMap::new(),
        }
    }
}

impl<P: LLMProvider + Clone + 'static> SessionBuilder<P> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn storage<S: SessionStorage + 'static>(mut self, storage: S) -> Self {
        self.storage = Some(Arc::new(storage));
        self
    }

    /// Normal path: hand the session a root [`AgentSpec`]; the whole subagent
    /// tree is derived from it.
    ///
    /// Mutually exclusive with [`SessionBuilder::agents`] — whichever is set
    /// last wins.
    pub fn root(mut self, spec: AgentSpec) -> Self {
        self.pending_spec = Some(spec);
        self.source = None;
        self
    }

    /// Required when using [`SessionBuilder::root`]; ignored with
    /// [`SessionBuilder::agents`].
    pub fn build_context(mut self, ctx: BuildContext) -> Self {
        self.build_context = Some(ctx);
        self
    }

    /// Escape hatch for advanced / test use: provide pre-built [`Agent`]
    /// instances and name the root. Bypasses `root` + `build_context`.
    pub fn agents(mut self, agents: HashMap<String, Agent>, root_name: impl Into<String>) -> Self {
        self.source = Some(AgentSource::PreBuilt {
            agents,
            root_name: root_name.into(),
        });
        self.pending_spec = None;
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

        let source = match (self.source.take(), self.pending_spec.take()) {
            (Some(pre), _) => pre,
            (None, Some(spec)) => {
                let ctx = self
                    .build_context
                    .take()
                    .ok_or(OpenError::MissingField("build_context"))?;
                AgentSource::Spec { spec, ctx }
            }
            (None, None) => return Err(OpenError::MissingField("root")),
        };

        let (agents, root_name) = match source {
            AgentSource::Spec { spec, ctx } => {
                let root_name = spec.name.clone();
                let agents = spec.build(&ctx).map_err(OpenError::Build)?;
                (agents, root_name)
            }
            AgentSource::PreBuilt { agents, root_name } => {
                if !agents.contains_key(&root_name) {
                    return Err(OpenError::UnknownRoot(root_name));
                }
                (agents, root_name)
            }
        };

        let session_id = self
            .session_id
            .take()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        // Load resumed state BEFORE bootstrap so we can (a) surface root history
        // via `resumed_checkpoint` and (b) detect pending approvals on *any*
        // agent in the snapshot, not just the root.
        let snapshot = storage
            .load_session_snapshot(&session_id)
            .await
            .map_err(OpenError::Storage)?;

        let resumed_checkpoint = storage
            .load_checkpoint(&session_id)
            .await
            .map_err(OpenError::Storage)?;

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
                resumed_checkpoint,
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
        let ckpt = storage
            .load_checkpoint(&tid)
            .await
            .map_err(OpenError::Storage)?;
        if let Some(ckpt) = ckpt
            && let ResumePoint::PendingApproval {
                ref pending_approval_calls,
                ..
            } = ckpt.resume_point
                && !pending_approval_calls.is_empty() {
                    pending.push(PendingApproval {
                        thread_id: ckpt.thread_id,
                        agent_name: ckpt.agent_name,
                        calls: pending_approval_calls.iter().cloned().collect(),
                        suspended_at: ckpt.suspended_at,
                    });
                }
    }
    Ok(pending)
}

struct SessionInner {
    runtime: AgentRuntime,
    root_name: String,
    session_id: String,
    resumed_checkpoint: Option<AgentCheckpoint>,
    has_resuming_agents: bool,
    events_rx: Mutex<broadcast::Receiver<(String, ThreadId, AgentEvent)>>,
}

/// High-level handle to a running agent session.
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

impl Session {
    pub fn builder<P: LLMProvider + Clone + 'static>() -> SessionBuilder<P> {
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

    /// The root agent's checkpoint at open time, if one was on disk. Intended
    /// for callers that want to render prior conversation history; pending
    /// approvals (on root or any sub-agent) are handled at `open` time via
    /// [`SessionBuilder::resume_decisions`], so this checkpoint's
    /// `resume_point` is never `PendingApproval` once `open` succeeds.
    pub fn resumed_checkpoint(&self) -> Option<&AgentCheckpoint> {
        self.inner.resumed_checkpoint.as_ref()
    }

    /// Send a user task to the root agent.
    pub async fn send(&self, task: impl Into<String>) -> Result<(), SendCommandError> {
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
                body: EnvelopeBody::Task(task),
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

    /// Receive the next session event. `None` once the runtime has shut down
    /// and all events have been drained.
    ///
    /// Lagged receivers drop overflowed events (logged via `tracing::warn`)
    /// rather than block runtime emitters.
    pub async fn recv(&self) -> Option<SessionEvent> {
        let mut rx = self.inner.events_rx.lock().await;
        loop {
            match rx.recv().await {
                Ok(raw) => return Some(self.wrap_event(raw)),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("session event stream lagged by {n} events; dropped");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
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
                let ok = self.inner.runtime.wait_for_exit(Some(timeout)).await;
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
