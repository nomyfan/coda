mod driver;
mod turn;

use crate::agent::EnvelopeBody;
use crate::persist::{StoredCheckpoint, StoredRuntimeSnapshot};
use crate::{Agent, AgentEvent, Envelope, ResumeDecision, RunConfig, Sender, ThreadId};
use coda_core::llm::{LLMProvider, TurnId};
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
use turn::{CallLedger, TurnAlreadyActive, TurnGate};

#[derive(Clone)]
enum AgentControl {
    Abort,
    /// Shutdown the agent gracefully.
    Exit,
}

#[derive(Debug)]
pub enum SendCommandError {
    TurnAlreadyActive,
    AgentNotFound,
    ChannelClosed,
}

impl std::fmt::Display for SendCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendCommandError::TurnAlreadyActive => write!(f, "A turn is already active"),
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

/// Every envelope [`AgentRuntime::bootstrap`] is about to put back. Within an
/// agent, inbox contents precede messages captured during the final drain;
/// agent names are sorted to keep recovery validation deterministic.
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
    turn_gate: Arc<TurnGate>,
    calls: Arc<CallLedger>,
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
            turn_gate: Arc::new(TurnGate::default()),
            calls: Arc::new(CallLedger::default()),
        }
    }

    /// The session's root thread, the only one whose turn endings end the turn
    /// itself — a sub-agent thread finishes its own work many times within one.
    pub(crate) fn is_root_thread(&self, thread_id: &ThreadId) -> bool {
        thread_id.as_ref() == self.session_id
    }

    /// Whether the turn a parked thread belongs to has been asked to stop. Its
    /// own agent cannot answer this — it is sitting idle with no turn in hand —
    /// so the thread's last stored message names the turn instead.
    pub(crate) async fn thread_turn_cancelled(&self, thread_id: &ThreadId) -> bool {
        match self.turn_of_thread(thread_id.as_ref()).await {
            Some(turn) => self.turn_gate.is_cancelled(turn),
            None => false,
        }
    }

    /// Ask a named turn to stop when a thread receives overlapping internal
    /// work it cannot safely start yet.
    ///
    /// The broadcast matters as much as the mark: a thread parked on an
    /// approval has no envelope coming to wake it, so without a nudge it would
    /// sit there while the turn it belongs to waits to be wound up.
    pub(crate) async fn cancel_turn(&self, turn: TurnId) {
        self.turn_gate.cancel(turn);
        self.broadcast_command(AgentControl::Abort).await;
    }

    async fn checkpoint_of(&self, thread_id: &str) -> Option<StoredCheckpoint> {
        self.session_storage
            .load_checkpoint(thread_id)
            .await
            .ok()
            .flatten()
    }

    /// The turn a stored thread was last working on, or `None` if it has no
    /// history to name one.
    async fn turn_of_thread(&self, thread_id: &str) -> Option<TurnId> {
        self.checkpoint_of(thread_id)
            .await?
            .messages
            .last()
            .map(|entry| entry.turn_id)
    }

    /// Put back the turn and the outstanding calls this process is about to
    /// resume, before any agent can see an envelope — an abort right after a
    /// restart has to find the turn, and internal inbox arbitration has to find
    /// the calls.
    ///
    /// Only work that will really run counts: threads the snapshot parked
    /// mid-turn, and envelopes about to be replayed. A turn nothing will pick up
    /// again must stay out of the active slot, or the session would reject new
    /// work forever.
    ///
    /// The call ledger is rebuilt by the rule that keeps it balanced: register
    /// one obligation for every [`CallLedger::end`] still to come. Those are a
    /// reply already in flight, a thread whose stored checkpoint still names a
    /// reply target, and a dispatched call its recipient has not picked up yet.
    /// Replayed envelopes go straight into an agent's inbox rather than through
    /// [`Self::send_message`], so this is the only place that can record them.
    async fn register_resumed_work(&self, snapshot: &AgentRuntimeSnapshot) -> Result<(), String> {
        // Active threads and replayed envelopes are independent evidence for
        // the same turn; `TurnGate::restore` verifies that they agree.
        for thread_id in snapshot.active_threads.values() {
            let Some(checkpoint) = self.checkpoint_of(thread_id).await else {
                continue;
            };
            if let Some(turn) = checkpoint.messages.last().map(|entry| entry.turn_id) {
                self.turn_gate.restore(turn)?;
            }
            // A stored reply target outlives only an unanswered call: it is
            // taken before the checkpoint that precedes the reply, so a thread
            // that already answered has none.
            if checkpoint.reply_target.is_some() {
                self.calls.begin(&ThreadId::from(thread_id.clone()));
            }
        }
        let replayed = replayed_envelopes(snapshot);
        for envelope in &replayed {
            match &envelope.body {
                EnvelopeBody::Task { message_id, .. } => {
                    self.turn_gate.restore(TurnId::from(*message_id))?;
                }
                EnvelopeBody::ToolCall { turn_id, .. } => {
                    self.turn_gate.restore(*turn_id)?;
                    self.calls.begin(&envelope.to.thread_id);
                }
                EnvelopeBody::Reply { .. } => {
                    if let Some(turn) = self.turn_of_thread(envelope.to.thread_id.as_ref()).await {
                        self.turn_gate.restore(turn)?;
                    }
                    // An answer nobody has taken yet still settles an obligation
                    // when its caller does take it.
                    if let Sender::Agent { thread_id, .. } = &envelope.from {
                        self.calls.begin(thread_id);
                    }
                }
                EnvelopeBody::Resume(_) => {
                    if let Some(turn) = self.turn_of_thread(envelope.to.thread_id.as_ref()).await {
                        self.turn_gate.restore(turn)?;
                    }
                }
            }
        }
        Ok(())
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
    ) -> Result<(), String> {
        if let Some(snapshot) = snapshot.as_ref() {
            self.register_resumed_work(snapshot).await?;
        }
        for (name, agent) in agents {
            info!("Bootstrap agent: {}", name);
            let runtime = self.clone();

            let agent_name = name.clone();
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
                agent_name
            });

            let handle = AgentHandle {
                control_sender: control_tx,
                message_sender: envelope_tx,
            };
            self.agents.lock().await.insert(name, handle);
        }
        Ok(())
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

    /// Stop the active turn on the user's behalf.
    ///
    /// It is marked before the broadcast leaves, because which agent picks the
    /// control message up first is not something the runtime can order —
    /// whoever gets there first must already find the mark in place.
    pub(crate) async fn request_abort(&self) {
        self.turn_gate.cancel_active();
        self.broadcast_command(AgentControl::Abort).await;
    }

    /// Request this runtime to exit all agent loops.
    pub(crate) async fn request_exit(&self) {
        self.exit_barrier.enter_exiting();
        self.broadcast_command(AgentControl::Exit).await;
    }

    /// Send a message to a specific agent, registering the turn it opens first
    /// so nothing can act on the envelope before the turn is on the books.
    /// A delivery that then fails takes back exactly what it added.
    pub(crate) async fn send_message(&self, envelope: Envelope) -> Result<(), SendCommandError> {
        let opened = if let EnvelopeBody::Task { message_id, .. } = &envelope.body {
            let turn = TurnId::from(*message_id);
            self.turn_gate
                .open(turn)
                .map_err(|TurnAlreadyActive| SendCommandError::TurnAlreadyActive)?;
            Some(turn)
        } else {
            None
        };
        let dispatched = matches!(envelope.body, EnvelopeBody::ToolCall { .. })
            .then(|| envelope.to.thread_id.clone());
        if let Some(thread_id) = &dispatched {
            self.calls.begin(thread_id);
        }
        let sent = self.deliver(envelope).await;
        if sent.is_err() {
            if let Some(turn) = opened {
                self.turn_gate.close(turn);
            }
            if let Some(thread_id) = &dispatched {
                self.calls.end(thread_id);
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

    /// Give the agents `duration` to exit on their own, and report whether they
    /// did. Non-destructive by design: a straggler stays running for the caller
    /// to cancel — dropping it here would leave the cancellation nothing to
    /// reach, and an in-flight turn no chance to save what it had.
    pub(crate) async fn wait_for_settle(&self, duration: Duration) -> bool {
        let mut agent_tasks = self.agent_tasks.lock().await;
        if agent_tasks.is_empty() {
            return true;
        }
        if timeout(duration, Self::drain(&mut agent_tasks))
            .await
            .is_err()
        {
            // The stragglers still own their snapshots; the final write
            // belongs to whoever ends them.
            return false;
        }
        self.persist_snapshot(Some(duration)).await;
        true
    }

    /// Wait for all bootstrapped agent tasks to exit, and return whether they
    /// managed it on their own.
    ///
    /// **No agent task is running when this returns with a deadline set.** Past
    /// the deadline the stragglers are aborted rather than waited on, because
    /// the states that make a task outstay a shutdown — a checkpoint write that
    /// never answers, an LLM stream that never ends — are exactly the ones no
    /// signal reaches: neither `Exit` nor `Abort` can interrupt a future the
    /// task is already awaiting. Callers depend on that guarantee rather than on
    /// the return value, since reopening a session while a task is still writing
    /// races its own read of the state.
    ///
    /// `None` asks for no deadline at all, and gives up the guarantee with it —
    /// only for callers that would rather hang than cut an in-flight turn short.
    pub(crate) async fn wait_for_exit(&self, timeout_duration: Option<Duration>) -> bool {
        let mut agent_tasks = self.agent_tasks.lock().await;
        if agent_tasks.is_empty() {
            return true;
        }

        let ret = match timeout_duration {
            Some(duration) => timeout(duration, Self::drain(&mut agent_tasks))
                .await
                .is_ok(),
            None => {
                Self::drain(&mut agent_tasks).await;
                true
            }
        };
        if !ret {
            warn!("aborting agent tasks that outstayed the shutdown deadline");
            agent_tasks.abort_all();
            while agent_tasks.join_next().await.is_some() {}
        }
        self.persist_snapshot(timeout_duration).await;

        ret
    }

    async fn drain(agent_tasks: &mut JoinSet<String>) {
        while let Some(result) = agent_tasks.join_next().await {
            match result {
                Ok(agent_name) => info!("Agent {} exited", agent_name),
                Err(err) => warn!("Agent task failed to join: {}", err),
            }
        }
    }

    /// Persist the accumulated snapshot as the runtime winds down. A deadline
    /// bounds this write too: storage that stopped answering is one way the
    /// wait ran out, and an unbounded write would pin the shutdown on the
    /// backend it just gave up on — with the session's key locked behind it.
    async fn persist_snapshot(&self, bound: Option<Duration>) {
        let save = async {
            self.session_storage
                .save_session_snapshot(
                    self.session_id.clone(),
                    self.snapshot.lock().await.clone().into(),
                )
                .await
        };
        let saved = match bound {
            Some(duration) => timeout(duration, save)
                .await
                .unwrap_or_else(|_| Err("the write outstayed the shutdown deadline".into())),
            None => save.await,
        };
        if let Err(err) = saved {
            warn!("Failed to persist session snapshot: {}", err);
        }
    }
}
