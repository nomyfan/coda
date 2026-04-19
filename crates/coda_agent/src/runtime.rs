mod driver;

use crate::{Agent, AgentCheckpoint, AgentEvent, Envelope, RunConfig, ThreadId};
use coda_core::llm::LLMProvider;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};
use tracing::{info, warn};

#[derive(Clone)]
enum AgentControl {
    Abort,
    /// Shutdown the agent gracefully.
    Exit,
}

#[derive(Debug)]
pub enum SendCommandError {
    AgentNotFound,
    ChannelClosed,
}

impl std::fmt::Display for SendCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendCommandError::AgentNotFound => write!(f, "Agent not found"),
            SendCommandError::ChannelClosed => write!(f, "Channel closed"),
        }
    }
}

impl std::error::Error for SendCommandError {}

struct AgentHandle {
    control_sender: mpsc::Sender<AgentControl>,
    message_sender: mpsc::Sender<Envelope>,
}

impl AgentHandle {
    async fn send_command(&self, cmd: AgentControl) -> Result<(), SendCommandError> {
        self.control_sender
            .send(cmd)
            .await
            .map_err(|_| SendCommandError::ChannelClosed)
    }

    /// Send a message to this agent, triggering a new turn.
    pub(crate) async fn send_message(&self, envelope: Envelope) -> Result<(), SendCommandError> {
        self.message_sender
            .send(envelope)
            .await
            .map_err(|_| SendCommandError::ChannelClosed)
    }
}

pub trait SessionStorage: Send + Sync {
    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: AgentCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentCheckpoint>, String>> + Send + '_>>;

    fn save_session_snapshot(
        &self,
        session_id: String,
        snapshot: AgentRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;

    fn load_session_snapshot(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentRuntimeSnapshot>, String>> + Send + '_>>;
}

impl SessionStorage for Arc<dyn SessionStorage> {
    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: AgentCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        (**self).save_checkpoint(thread_id, checkpoint)
    }

    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentCheckpoint>, String>> + Send + '_>> {
        (**self).load_checkpoint(thread_id)
    }

    fn save_session_snapshot(
        &self,
        session_id: String,
        snapshot: AgentRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        (**self).save_session_snapshot(session_id, snapshot)
    }

    fn load_session_snapshot(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentRuntimeSnapshot>, String>> + Send + '_>>
    {
        (**self).load_session_snapshot(session_id)
    }
}

#[derive(Clone, Default)]
pub struct MemoryStorage {
    checkpoints: Arc<Mutex<HashMap<String, AgentCheckpoint>>>,
    snapshots: Arc<Mutex<HashMap<String, AgentRuntimeSnapshot>>>,
}

impl SessionStorage for MemoryStorage {
    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: AgentCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.checkpoints.lock().await.insert(thread_id, checkpoint);
            Ok(())
        })
    }

    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentCheckpoint>, String>> + Send + '_>> {
        let thread_id = thread_id.to_owned();
        Box::pin(async move {
            let checkpoint = self.checkpoints.lock().await.get(&thread_id).cloned();
            Ok(checkpoint)
        })
    }

    fn save_session_snapshot(
        &self,
        session_id: String,
        snapshot: AgentRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.snapshots.lock().await.insert(session_id, snapshot);
            Ok(())
        })
    }

    fn load_session_snapshot(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentRuntimeSnapshot>, String>> + Send + '_>>
    {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            let snapshot = self.snapshots.lock().await.get(&session_id).cloned();
            Ok(snapshot)
        })
    }
}

#[derive(Clone, Default)]
pub(crate) struct ExitBarrier {
    inner: Arc<AtomicBool>,
}

impl ExitBarrier {
    fn enter_exiting(&self) -> bool {
        self.inner
            .compare_exchange(false, true, Ordering::Release, Ordering::Acquire)
            .is_ok()
    }

    fn is_exiting(&self) -> bool {
        self.inner.load(Ordering::Acquire)
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AgentRuntimeSnapshot {
    pub drained_envelopes: HashMap<String, Vec<Envelope>>,
    pub agent_drained_envelopes: HashMap<String, Vec<Envelope>>,
    pub active_threads: HashMap<String, String>,
}

#[derive(Clone)]
pub(crate) struct AgentRuntime {
    session_id: String,
    /// Key: unique agent name
    agents: Arc<Mutex<HashMap<String, AgentHandle>>>,
    agent_tasks: Arc<Mutex<JoinSet<String>>>,
    /// Global event bus — all agents forward their events here.
    global_event_tx: broadcast::Sender<(String, ThreadId, AgentEvent)>,
    session_storage: Arc<dyn SessionStorage>,
    exit_barrier: ExitBarrier,
    snapshot: Arc<Mutex<AgentRuntimeSnapshot>>,
}

impl AgentRuntime {
    pub(crate) fn new(session_storage: impl SessionStorage + 'static, session_id: String) -> Self {
        let (global_event_tx, _) = broadcast::channel(128);
        AgentRuntime {
            session_id,
            agents: Arc::new(Mutex::new(HashMap::new())),
            agent_tasks: Arc::new(Mutex::new(JoinSet::new())),
            global_event_tx,
            session_storage: Arc::new(session_storage),
            exit_barrier: ExitBarrier::default(),
            snapshot: Arc::new(Mutex::new(AgentRuntimeSnapshot::default())),
        }
    }

    pub(crate) async fn emit_event(
        &self,
        agent_name: String,
        thread_id: ThreadId,
        event: AgentEvent,
    ) {
        let _ = self.global_event_tx.send((agent_name, thread_id, event));
    }

    pub(crate) async fn bootstrap(
        &mut self,
        agents: HashMap<String, Agent>,
        mut snapshot: Option<AgentRuntimeSnapshot>,
        config: RunConfig<impl LLMProvider + Clone>,
    ) {
        for (name, agent) in agents {
            info!("Bootstrapping agent: {}", name);
            let runtime = self.clone();

            let task_name = name.clone();
            let config = config.clone();
            let active_thread = snapshot
                .as_ref()
                .and_then(|s| s.active_threads.get(&name))
                .map(|id| ThreadId(id.clone()));
            let init_envelopes = snapshot
                .as_mut()
                .and_then(|s| {
                    let mut first = s.agent_drained_envelopes.remove(&name).unwrap_or_default();
                    let second = s.drained_envelopes.remove(&name).unwrap_or_default();
                    first.extend(second);
                    Some(first)
                })
                .unwrap_or_default();

            let (control_tx, control_rx) = mpsc::channel(8);
            // For simplicity, we just replay the drained envelopes by putting them back to to agent's inbox.
            let (envelope_tx, envelope_rx) = mpsc::channel(8.max(init_envelopes.len() + 8));
            for envelope in init_envelopes {
                let _ = envelope_tx.send(envelope).await;
            }
            self.agent_tasks.lock().await.spawn(async move {
                driver::run_agent(
                    runtime,
                    active_thread,
                    agent,
                    control_rx,
                    envelope_rx,
                    config,
                )
                .await;
                task_name
            });

            let handle = AgentHandle {
                control_sender: control_tx,
                message_sender: envelope_tx,
            };
            self.agents.lock().await.insert(name, handle);
        }
    }

    /// Subscribe to events from all agents
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<(String, ThreadId, AgentEvent)> {
        self.global_event_tx.subscribe()
    }

    async fn broadcast_command(&self, cmd: AgentControl) {
        let agents = self.agents.lock().await;
        for entry in agents.values() {
            let err = entry.send_command(cmd.clone()).await;
            if let Err(e) = err {
                info!("Failed to send command to agent: {}", e);
            }
        }
    }

    /// Abort the current work for this runtime.
    pub(crate) async fn request_abort(&self) {
        self.broadcast_command(AgentControl::Abort).await;
    }

    /// Request this runtime to exit all agent loops.
    pub(crate) async fn request_exit(&self) {
        self.exit_barrier.enter_exiting();
        self.broadcast_command(AgentControl::Exit).await;
    }

    /// Send a message to a specific agent.
    pub(crate) async fn send_message(&self, envelope: Envelope) -> Result<(), SendCommandError> {
        if self.exit_barrier.is_exiting() {
            // During the suspension draining phase, we buffer incoming messages instead of sending them to agents.
            let receiver = envelope.to.name.clone();
            let mut snapshot = self.snapshot.lock().await;
            snapshot
                .drained_envelopes
                .entry(receiver.clone())
                .or_default()
                .push(envelope);
            return Ok(());
        }
        let agents = self.agents.lock().await;
        if let Some(handle) = agents.get(envelope.to.name.as_str()) {
            handle.send_message(envelope).await
        } else {
            Err(SendCommandError::AgentNotFound)
        }
    }

    pub(crate) async fn save_agent_snapshot(
        &self,
        agent_name: String,
        envelopes: Vec<Envelope>,
        active_thread: Option<ThreadId>,
    ) {
        let mut snapshot = self.snapshot.lock().await;
        snapshot
            .agent_drained_envelopes
            .insert(agent_name.clone(), envelopes);
        if let Some(thread_id) = active_thread {
            snapshot.active_threads.insert(agent_name, thread_id.0);
        }
        // TODO: snapshot is only persisted in wait_for_exit, so a crash between
        // suspension and clean exit loses drained_envelopes / active_threads.
        // For robust async HITL, persist the snapshot here (and in send_message's
        // buffering branch) so state survives process restarts.
    }

    /// Wait for all bootstrapped agent tasks to exit.
    ///
    /// Returns `false` if the timeout elapses before every agent stops.
    pub(crate) async fn wait_for_exit(&self, timeout_duration: Option<Duration>) -> bool {
        let mut agent_tasks = self.agent_tasks.lock().await;
        if agent_tasks.is_empty() {
            return true;
        }

        let wait_for_exit = async {
            while let Some(result) = agent_tasks.join_next().await {
                match result {
                    Ok(agent_name) => info!("Agent {} exited", agent_name),
                    Err(err) => warn!("Agent task failed to join: {}", err),
                }
            }
        };

        let ret = match timeout_duration {
            Some(duration) => timeout(duration, wait_for_exit).await.is_ok(),
            None => {
                wait_for_exit.await;
                true
            }
        };
        // TODO: abort any remaining agents if timeout occurs and return early, instead of waiting for them to exit on their own.
        // Root cause: graceful exit awaits each agent's current run_fut to completion, which deadlocks
        // when an agent is stuck on a slow/hung LLM stream or external API. Session::shutdown with
        // OnTimeout::Abort covers this at the session layer; at the runtime layer, callers must still
        // combine request_abort + request_exit manually to guarantee termination.
        if let Err(err) = self
            .session_storage
            .save_session_snapshot(self.session_id.clone(), self.snapshot.lock().await.clone())
            .await
        {
            warn!("Failed to persist session snapshot: {}", err);
        }

        ret
    }
}
