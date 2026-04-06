mod driver;

use crate::{Agent, AgentCheckpoint, AgentEvent, Envelope, RunConfig, ThreadId};
use coda_core::llm::LLMProvider;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};
use tracing::{info, warn};

#[derive(Clone)]
enum AgentControl {
    Abort,
    /// Shut down the agent loop entirely.
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
    pub async fn send_message(&self, envelope: Envelope) -> Result<(), SendCommandError> {
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
}

#[derive(Clone, Default)]
pub struct MemoryStorage {
    checkpoints: Arc<Mutex<HashMap<String, AgentCheckpoint>>>,
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
}

#[derive(Clone)]
pub struct AgentRuntime {
    /// Key: unique agent name
    agents: Arc<Mutex<HashMap<String, AgentHandle>>>,
    agent_tasks: Arc<Mutex<JoinSet<String>>>,
    /// Global event bus — all agents forward their events here.
    global_event_tx: broadcast::Sender<(String, ThreadId, AgentEvent)>,
    session_storage: Arc<dyn SessionStorage>,
}

impl AgentRuntime {
    pub fn new(session_storage: impl SessionStorage + 'static) -> Self {
        let (global_event_tx, _) = broadcast::channel(256);
        AgentRuntime {
            agents: Arc::new(Mutex::new(HashMap::new())),
            agent_tasks: Arc::new(Mutex::new(JoinSet::new())),
            global_event_tx,
            session_storage: Arc::new(session_storage),
        }
    }

    pub(crate) fn emit_event(&self, agent_name: String, thread_id: ThreadId, event: AgentEvent) {
        let _ = self.global_event_tx.send((agent_name, thread_id, event));
    }

    pub async fn bootstrap(
        &mut self,
        agents: HashMap<String, Agent>,
        config: RunConfig<impl LLMProvider + Clone>,
    ) {
        for (name, agent) in agents {
            info!("Bootstrapping agent: {}", name);
            let (control_tx, control_rx) = mpsc::channel(10);
            let (envelope_tx, envelope_rx) = mpsc::channel(10);
            let runtime = self.clone();

            let task_name = name.clone();
            let config = config.clone();
            self.agent_tasks.lock().await.spawn(async move {
                driver::run_agent(runtime, agent, control_rx, envelope_rx, config).await;
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
    pub fn subscribe(&self) -> broadcast::Receiver<(String, ThreadId, AgentEvent)> {
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
    pub async fn abort(&self) {
        self.broadcast_command(AgentControl::Abort).await;
    }

    /// Request this runtime to exit all agent loops.
    pub async fn exit(&self) {
        self.broadcast_command(AgentControl::Exit).await;
    }

    /// Send a message to a specific agent.
    pub async fn send_message(&self, envelope: Envelope) -> Result<(), SendCommandError> {
        let agents = self.agents.lock().await;
        if let Some(handle) = agents.get(envelope.to.name.as_str()) {
            handle.send_message(envelope).await
        } else {
            Err(SendCommandError::AgentNotFound)
        }
    }

    /// Wait for all bootstrapped agent tasks to exit.
    ///
    /// Returns `false` if the timeout elapses before every agent stops.
    pub async fn wait_for_exit(&self, timeout_duration: Option<Duration>) -> bool {
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

        match timeout_duration {
            Some(duration) => timeout(duration, wait_for_exit).await.is_ok(),
            None => {
                wait_for_exit.await;
                true
            }
        }
    }
}
