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
use crate::runtime::{AgentRuntime, SendCommandError, SessionStorage};
use crate::{
    Agent, AgentCheckpoint, AgentEvent, AgentSpec, BuildContext, BuildError, Envelope,
    ResumeDecision, RunConfig, Sender, ThreadId,
};
use coda_core::llm::LLMProvider;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};
use tokio::time::Duration;
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

        // Load resumed state BEFORE bootstrap so we can populate `resumed_checkpoint`
        // and `has_pending_approval` on the returned Session.
        let snapshot = storage
            .load_session_snapshot(&session_id)
            .await
            .map_err(OpenError::Storage)?;
        let resumed_checkpoint = storage
            .load_checkpoint(&session_id)
            .await
            .map_err(OpenError::Storage)?;
        // TODO: 这里的 has_pending_approval 语义有问题，应该是任意 agent 存在 pending approval 就是存在，应该可以和另外一个没有记录多个 agents 有 suspend 的问题一起修复。
        let has_pending_approval = matches!(
            resumed_checkpoint.as_ref().map(|c| &c.resume_point),
            Some(ResumePoint::PendingApproval { .. })
        );

        let mut runtime = AgentRuntime::new(storage, session_id.clone());
        // CRITICAL: subscribe before bootstrap so no events are lost between
        // spawn and the caller's first `recv`.
        let events_rx = runtime.subscribe();
        runtime.bootstrap(agents, snapshot, run_config).await;

        Ok(Session {
            inner: Arc::new(SessionInner {
                runtime,
                root_name,
                session_id,
                resumed_checkpoint,
                has_pending_approval,
                events_rx: Mutex::new(events_rx),
            }),
        })
    }
}

struct SessionInner {
    runtime: AgentRuntime,
    root_name: String,
    session_id: String,
    resumed_checkpoint: Option<AgentCheckpoint>,
    has_pending_approval: bool,
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

    /// `true` when the root agent's loaded checkpoint is sitting in
    /// `PendingApproval` — the event stream will emit a `Suspended` event
    /// without needing an initial `send`.
    pub fn has_pending_approval(&self) -> bool {
        self.inner.has_pending_approval
    }

    /// The root agent's checkpoint at open time, if one was on disk.
    pub fn resumed_checkpoint(&self) -> Option<&AgentCheckpoint> {
        self.inner.resumed_checkpoint.as_ref()
    }

    /// Escape hatch for advanced use (direct envelope sends, custom
    /// subscriptions, etc.).
    pub fn runtime(&self) -> &AgentRuntime {
        &self.inner.runtime
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

    /// Resume a suspended agent with the user's decision on its pending
    /// approvals. The envelope target is derived from `checkpoint` so the
    /// caller does not build one by hand.
    pub async fn resume(
        &self,
        checkpoint: &AgentCheckpoint,
        decision: ResumeDecision,
    ) -> Result<(), SendCommandError> {
        let agent_name = checkpoint.agent_name.clone();
        let thread_id = ThreadId::from(checkpoint.thread_id.clone());
        self.inner
            .runtime
            .send_message(Envelope::with_id(|id| Envelope {
                id,
                from: Sender::User,
                to: Receiver {
                    name: agent_name,
                    thread_id,
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
        // TODO: 处理多个 agent 触发 suspend 的场景
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
