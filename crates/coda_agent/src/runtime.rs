mod driver;

use crate::agent::EnvelopeBody;
use crate::persist::{StoredCheckpoint, StoredRuntimeSnapshot};
use crate::{Agent, AgentEvent, Envelope, ResumeDecision, RunConfig, ThreadId};
use coda_core::llm::{LLMProvider, TurnId};
use std::collections::{HashMap, HashSet};
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
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredCheckpoint>, String>> + Send + '_>>;

    fn save_session_snapshot(
        &self,
        session_id: String,
        snapshot: StoredRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;

    fn load_session_snapshot(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredRuntimeSnapshot>, String>> + Send + '_>>;
}

impl SessionStorage for Arc<dyn SessionStorage> {
    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        (**self).save_checkpoint(thread_id, checkpoint)
    }

    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredCheckpoint>, String>> + Send + '_>> {
        (**self).load_checkpoint(thread_id)
    }

    fn save_session_snapshot(
        &self,
        session_id: String,
        snapshot: StoredRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        (**self).save_session_snapshot(session_id, snapshot)
    }

    fn load_session_snapshot(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredRuntimeSnapshot>, String>> + Send + '_>>
    {
        (**self).load_session_snapshot(session_id)
    }
}

#[derive(Clone, Default)]
pub struct MemoryStorage {
    checkpoints: Arc<Mutex<HashMap<String, StoredCheckpoint>>>,
    snapshots: Arc<Mutex<HashMap<String, StoredRuntimeSnapshot>>>,
}

impl MemoryStorage {
    /// Every checkpoint written so far, sorted by thread id. For assertions
    /// about a session as a whole (its thread tree), where the caller cannot
    /// name the threads up front because their ids are derived.
    pub async fn all_checkpoints(&self) -> Vec<StoredCheckpoint> {
        let mut checkpoints: Vec<_> = self.checkpoints.lock().await.values().cloned().collect();
        checkpoints.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
        checkpoints
    }
}

impl SessionStorage for MemoryStorage {
    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.checkpoints.lock().await.insert(thread_id, checkpoint);
            Ok(())
        })
    }

    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredCheckpoint>, String>> + Send + '_>> {
        let thread_id = thread_id.to_owned();
        Box::pin(async move {
            let checkpoint = self.checkpoints.lock().await.get(&thread_id).cloned();
            Ok(checkpoint)
        })
    }

    fn save_session_snapshot(
        &self,
        session_id: String,
        snapshot: StoredRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.snapshots.lock().await.insert(session_id, snapshot);
            Ok(())
        })
    }

    fn load_session_snapshot(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredRuntimeSnapshot>, String>> + Send + '_>>
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

#[derive(Debug, Default, Clone)]
pub struct AgentRuntimeSnapshot {
    pub drained_envelopes: HashMap<String, Vec<Envelope>>,
    pub agent_drained_envelopes: HashMap<String, Vec<Envelope>>,
    pub active_threads: HashMap<String, String>,
}

/// Every envelope [`AgentRuntime::bootstrap`] is about to put back, in the
/// order the agents will see them: per agent, whatever was still in its inbox
/// before whatever arrived during the drain. Agent names are sorted only to
/// make the walk deterministic — the order that carries meaning is the one
/// inside a single agent's queue, since only the root's holds submissions.
fn replayed_envelopes(snapshot: &AgentRuntimeSnapshot) -> Vec<&Envelope> {
    let mut names: Vec<&String> = snapshot
        .agent_drained_envelopes
        .keys()
        .chain(snapshot.drained_envelopes.keys())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .flat_map(|name| {
            snapshot
                .agent_drained_envelopes
                .get(name)
                .into_iter()
                .flatten()
                .chain(snapshot.drained_envelopes.get(name).into_iter().flatten())
        })
        .collect()
}

/// The turns this session has been asked to run and has not finished, oldest
/// first, alongside the ones something has asked to stop.
///
/// Only a user `Task` opens a turn — a `ToolCall` carries its caller's turn id,
/// and `Reply`/`Resume` continue work that is already registered. That is what
/// makes one flat list both complete and correctly ordered even though each
/// agent's inbox is queued independently: new turns only ever arrive through
/// the root agent's inbox, so there is only one submission order to preserve.
#[derive(Default)]
struct ActiveTurns {
    order: Vec<TurnId>,
    cancelled: HashSet<TurnId>,
}

#[derive(Clone)]
pub(crate) struct AgentRuntime {
    session_id: String,
    /// Key: unique agent name
    agents: Arc<Mutex<HashMap<String, AgentHandle>>>,
    agent_tasks: Arc<Mutex<JoinSet<String>>>,
    /// Global event bus — all agents forward their events here.
    global_event_tx: broadcast::Sender<(String, ThreadId, TurnId, AgentEvent)>,
    session_storage: Arc<dyn SessionStorage>,
    exit_barrier: ExitBarrier,
    snapshot: Arc<Mutex<AgentRuntimeSnapshot>>,
    /// A blocking mutex on purpose: every critical section below is a handful
    /// of vector operations and none of them awaits.
    turns: Arc<std::sync::Mutex<ActiveTurns>>,
    /// Calls dispatched to a thread that has not been answered yet, counted per
    /// thread.
    ///
    /// The count spans the whole obligation — from the call going out to the
    /// caller consuming the answer — rather than just the wait in the inbox. A
    /// thread that already took its envelope and is now itself waiting on a
    /// sub-agent of its own is still working, and treating it as gone is what
    /// would make a caller write its result for it.
    unanswered: Arc<std::sync::Mutex<HashMap<ThreadId, usize>>>,
}

impl AgentRuntime {
    pub(crate) fn new(session_storage: impl SessionStorage + 'static, session_id: String) -> Self {
        // Sized for chunk-level event bursts (LLM streaming): a lagged receiver
        // drops events, which consumers can only partially recover from. A
        // margin against scheduling delays, not a response to an observed failure.
        let (global_event_tx, _) = broadcast::channel(256);
        AgentRuntime {
            session_id,
            agents: Arc::new(Mutex::new(HashMap::new())),
            agent_tasks: Arc::new(Mutex::new(JoinSet::new())),
            global_event_tx,
            session_storage: Arc::new(session_storage),
            exit_barrier: ExitBarrier::default(),
            snapshot: Arc::new(Mutex::new(AgentRuntimeSnapshot::default())),
            turns: Arc::new(std::sync::Mutex::new(ActiveTurns::default())),
            unanswered: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// The session's root thread, the only one whose turn endings end the turn
    /// itself — a sub-agent thread finishes its own work many times within one.
    pub(crate) fn is_root_thread(&self, thread_id: &ThreadId) -> bool {
        thread_id.as_ref() == self.session_id
    }

    /// Append a turn to the active list. Idempotent, which is what lets
    /// recovery register the same turn from several angles — and lets a runtime
    /// snapshot that gets replayed twice still describe one turn.
    fn register_turn(&self, turn: TurnId) -> bool {
        let mut turns = self.turns.lock().expect("active turns");
        if turns.order.contains(&turn) {
            return false;
        }
        turns.order.push(turn);
        true
    }

    /// Register the turn an envelope opens, before anything can act on it.
    /// Returns the id only when a new turn was registered, so a delivery that
    /// then fails can take back exactly what it added.
    fn open_turn(&self, envelope: &Envelope) -> Option<TurnId> {
        let EnvelopeBody::Task { message_id, .. } = &envelope.body else {
            return None;
        };
        let turn = TurnId::from(*message_id);
        self.register_turn(turn).then_some(turn)
    }

    /// Drop a finished turn. Idempotent — a turn that was never registered, or
    /// was already closed, is simply absent.
    pub(crate) fn close_turn(&self, turn: TurnId) {
        let mut turns = self.turns.lock().expect("active turns");
        turns.order.retain(|active| *active != turn);
        turns.cancelled.remove(&turn);
    }

    /// Whether this turn has been asked to stop. Agents read the mark rather
    /// than deciding for themselves which turn an abort meant: a stateless
    /// agent's `Agent` instance is reused across threads, so while it sits idle
    /// its own `current_turn()` still names the previous one.
    pub(crate) fn is_cancelled(&self, turn: TurnId) -> bool {
        self.turns
            .lock()
            .expect("active turns")
            .cancelled
            .contains(&turn)
    }

    /// Whether the turn a parked thread belongs to has been asked to stop. Its
    /// own agent cannot answer this — it is sitting idle with no turn in hand —
    /// so the thread's last stored message names the turn instead.
    pub(crate) async fn thread_turn_cancelled(&self, thread_id: &ThreadId) -> bool {
        match self.turn_of_thread(thread_id.as_ref()).await {
            Some(turn) => self.is_cancelled(turn),
            None => false,
        }
    }

    /// Note that a call has gone out to `thread_id` and has not been answered.
    fn begin_call(&self, thread_id: &ThreadId) {
        *self
            .unanswered
            .lock()
            .expect("unanswered calls")
            .entry(thread_id.clone())
            .or_insert(0) += 1;
    }

    /// Note that one of `thread_id`'s callers has taken its answer.
    pub(crate) fn end_call(&self, thread_id: &ThreadId) {
        let mut unanswered = self.unanswered.lock().expect("unanswered calls");
        if let Some(count) = unanswered.get_mut(thread_id) {
            *count -= 1;
            if *count == 0 {
                unanswered.remove(thread_id);
            }
        }
    }

    /// Whether this thread still owes somebody an answer in this process.
    ///
    /// `false` means nothing here will ever produce that answer — the work went
    /// away with a previous process — and the caller is free to write the call
    /// off rather than wait forever.
    pub(crate) fn is_answering(&self, thread_id: &ThreadId) -> bool {
        self.unanswered
            .lock()
            .expect("unanswered calls")
            .contains_key(thread_id)
    }

    /// Ask a named turn to stop, without touching the queue it sits in. Used
    /// when a new submission supersedes the one in flight.
    ///
    /// The broadcast matters as much as the mark: a thread parked on an
    /// approval has no envelope coming to wake it, so without a nudge it would
    /// sit there while the turn it belongs to waits to be wound up.
    pub(crate) async fn cancel_turn(&self, turn: TurnId) {
        self.turns
            .lock()
            .expect("active turns")
            .cancelled
            .insert(turn);
        self.broadcast_command(AgentControl::Abort).await;
    }

    /// The turn a stored thread was last working on, or `None` if it has no
    /// history to name one.
    async fn turn_of_thread(&self, thread_id: &str) -> Option<TurnId> {
        self.session_storage
            .load_checkpoint(thread_id)
            .await
            .ok()
            .flatten()?
            .messages
            .last()
            .map(|entry| entry.turn_id)
    }

    /// Put back the turns this process is about to resume, before any agent can
    /// see an envelope — an abort right after a restart has to find them.
    ///
    /// Only work that will really run counts: threads the snapshot parked
    /// mid-turn, and envelopes about to be replayed. A turn nothing will pick up
    /// again must stay off the list, or it would sit at the head forever and
    /// swallow every abort aimed at the turns behind it.
    async fn register_resumed_turns(&self, snapshot: &AgentRuntimeSnapshot) {
        // In-flight work first: whatever was already running is older than
        // anything queued behind it.
        for thread_id in snapshot.active_threads.values() {
            if let Some(turn) = self.turn_of_thread(thread_id).await {
                self.register_turn(turn);
            }
        }
        let replayed = replayed_envelopes(snapshot);
        for envelope in &replayed {
            match &envelope.body {
                // A submission is queued behind the in-flight work, so it waits
                // for the second pass.
                EnvelopeBody::Task { .. } => {}
                EnvelopeBody::ToolCall { turn_id, .. } => {
                    self.register_turn(*turn_id);
                }
                EnvelopeBody::Reply { .. } | EnvelopeBody::Resume(_) => {
                    if let Some(turn) = self.turn_of_thread(envelope.to.thread_id.as_ref()).await {
                        self.register_turn(turn);
                    }
                }
            }
        }
        for envelope in &replayed {
            if let EnvelopeBody::Task { message_id, .. } = &envelope.body {
                self.register_turn(TurnId::from(*message_id));
            }
        }
    }

    /// Publish an event, tagged with the turn it belongs to. One turn id spans
    /// the whole sub-tree it reaches, which is what lets a consumer settle
    /// per-turn without reconstructing who called whom.
    pub(crate) async fn emit_event(
        &self,
        agent_name: String,
        thread_id: ThreadId,
        turn_id: TurnId,
        event: AgentEvent,
    ) {
        let _ = self
            .global_event_tx
            .send((agent_name, thread_id, turn_id, event));
    }

    pub(crate) async fn bootstrap(
        &mut self,
        agents: HashMap<String, Agent>,
        mut snapshot: Option<AgentRuntimeSnapshot>,
        mut resume_decisions: HashMap<String, ResumeDecision>,
        config: RunConfig<impl LLMProvider + Clone>,
    ) {
        if let Some(snapshot) = snapshot.as_ref() {
            self.register_resumed_turns(snapshot).await;
        }
        for (name, agent) in agents {
            info!("Bootstrap agent: {}", name);
            let runtime = self.clone();

            let task_name = name.clone();
            let agent_config = config.resolve(&name);
            let active_thread = snapshot
                .as_ref()
                .and_then(|s| s.active_threads.get(&name))
                .map(|id| ThreadId(id.clone()));
            let resume_decision = active_thread
                .as_ref()
                .and_then(|tid| resume_decisions.remove(tid.as_ref()));
            let init_envelopes = snapshot
                .as_mut()
                .map(|s| {
                    let mut first = s.agent_drained_envelopes.remove(&name).unwrap_or_default();
                    let second = s.drained_envelopes.remove(&name).unwrap_or_default();
                    first.extend(second);
                    first
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
                    (active_thread, resume_decision),
                    agent,
                    control_rx,
                    envelope_rx,
                    agent_config,
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
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<(String, ThreadId, TurnId, AgentEvent)> {
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

    /// Cancel whatever is running without marking any turn.
    ///
    /// This is teardown, not the user taking a turn back: a thread parked on an
    /// approval stays parked, so the pending decision survives into the next
    /// process instead of being written off on the way out.
    pub(crate) async fn cancel_in_flight(&self) {
        self.broadcast_command(AgentControl::Abort).await;
    }

    /// Stop the turn at the head of the queue on the user's behalf.
    ///
    /// It is marked before the broadcast leaves, because which agent picks the
    /// control message up first is not something the runtime can order —
    /// whoever gets there first must already find the mark in place.
    pub(crate) async fn request_abort(&self) {
        {
            let mut turns = self.turns.lock().expect("active turns");
            if let Some(head) = turns.order.first().copied() {
                turns.cancelled.insert(head);
            }
        }
        self.broadcast_command(AgentControl::Abort).await;
    }

    /// Request this runtime to exit all agent loops.
    pub(crate) async fn request_exit(&self) {
        self.exit_barrier.enter_exiting();
        self.broadcast_command(AgentControl::Exit).await;
    }

    /// Send a message to a specific agent, registering the turn it opens first
    /// so nothing can act on the envelope before the turn is on the books.
    pub(crate) async fn send_message(&self, envelope: Envelope) -> Result<(), SendCommandError> {
        let opened = self.open_turn(&envelope);
        let dispatched = matches!(envelope.body, EnvelopeBody::ToolCall { .. })
            .then(|| envelope.to.thread_id.clone());
        if let Some(thread_id) = &dispatched {
            self.begin_call(thread_id);
        }
        let sent = self.deliver(envelope).await;
        if sent.is_err() {
            if let Some(turn) = opened {
                self.close_turn(turn);
            }
            if let Some(thread_id) = &dispatched {
                self.end_call(thread_id);
            }
        }
        sent
    }

    async fn deliver(&self, envelope: Envelope) -> Result<(), SendCommandError> {
        if self.exit_barrier.is_exiting() {
            // During the exit draining phase, buffer incoming messages and persist
            // immediately so they survive a crash before shutdown completes.
            let receiver = envelope.to.name.clone();
            let mut snapshot = self.snapshot.lock().await;
            snapshot
                .drained_envelopes
                .entry(receiver.clone())
                .or_default()
                .push(envelope);
            if let Err(err) = self
                .session_storage
                .save_session_snapshot(self.session_id.clone(), snapshot.clone().into())
                .await
            {
                warn!(
                    "Failed to persist session snapshot on buffered message: {}",
                    err
                );
            }
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
        match active_thread {
            Some(thread_id) => {
                snapshot.active_threads.insert(agent_name, thread_id.0);
            }
            None => {
                snapshot.active_threads.remove(&agent_name);
            }
        }
        if let Err(err) = self
            .session_storage
            .save_session_snapshot(self.session_id.clone(), snapshot.clone().into())
            .await
        {
            warn!("Failed to persist session snapshot: {}", err);
        }
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
            .save_session_snapshot(
                self.session_id.clone(),
                self.snapshot.lock().await.clone().into(),
            )
            .await
        {
            warn!("Failed to persist session snapshot: {}", err);
        }

        ret
    }
}
