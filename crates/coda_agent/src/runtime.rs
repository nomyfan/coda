mod cleanup;
mod driver;
mod notices;
mod scopes;
mod turn;

use crate::agent::EnvelopeBody;
use crate::persist::{StoredCheckpoint, StoredResumePoint, StoredRuntimeSnapshot};
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
    StopScope,
    /// Shutdown the agent gracefully.
    Exit,
}

#[derive(Debug)]
pub enum SendCommandError {
    TurnAlreadyActive,
    ThreadBusy,
    AwaitingCleanup,
    PendingApprovals,
    ScopeClosed,
    StaleApproval,
    AgentNotFound,
    ChannelClosed,
}

impl std::fmt::Display for SendCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendCommandError::TurnAlreadyActive => write!(f, "A turn is already active"),
            SendCommandError::ThreadBusy => write!(
                f,
                "Subagent thread is busy; wait for its current call to finish"
            ),
            SendCommandError::AwaitingCleanup => write!(f, "Thread is waiting for abort cleanup"),
            SendCommandError::PendingApprovals => write!(f, "Session has pending approvals"),
            SendCommandError::StaleApproval => {
                write!(f, "Approval is stale or does not match its pending calls")
            }
            SendCommandError::ScopeClosed => write!(f, "Execution scope is closed"),
            SendCommandError::AgentNotFound => write!(f, "Agent not found"),
            SendCommandError::ChannelClosed => write!(f, "Channel closed"),
        }
    }
}

impl std::error::Error for SendCommandError {}

#[derive(Clone)]
struct AgentHandle {
    control_sender: mpsc::Sender<AgentControl>,
    message_sender: mpsc::Sender<Envelope>,
    abort: tokio::task::AbortHandle,
    finished: tokio::sync::watch::Receiver<bool>,
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
    fn has_notice_receipt(
        &self,
        _task_id: coda_core::task::TaskId,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + '_>> {
        Box::pin(async { Ok(false) })
    }
    fn admit_task_notice(
        &self,
        _task_id: coda_core::task::TaskId,
        _checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Err("storage does not support durable notices".into()) })
    }

    fn load_background_checkpoints(
        &self,
        _session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<StoredCheckpoint>, String>> + Send + '_>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn abort_scope(
        &self,
        _scope: crate::execution::ScopeAbort,
    ) -> Pin<Box<dyn Future<Output = Result<crate::execution::CleanupReceipt, String>> + Send + '_>>
    {
        Box::pin(async { Err("storage does not support scope cleanup".into()) })
    }
    fn save_execution_checkpoint(
        &self,
        _identity: crate::execution::ExecutionIdentity,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.save_checkpoint(checkpoint.thread_id.clone(), checkpoint)
    }

    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredCheckpoint>, String>> + Send + '_>>;

    /// Load every checkpoint in this session that is waiting for tool approval.
    fn load_pending_approval_checkpoints(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<StoredCheckpoint>, String>> + Send + '_>>;

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
    fn has_notice_receipt(
        &self,
        task_id: coda_core::task::TaskId,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + '_>> {
        (**self).has_notice_receipt(task_id)
    }
    fn admit_task_notice(
        &self,
        task_id: coda_core::task::TaskId,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        (**self).admit_task_notice(task_id, checkpoint)
    }

    fn load_background_checkpoints(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<StoredCheckpoint>, String>> + Send + '_>> {
        (**self).load_background_checkpoints(session_id)
    }

    fn abort_scope(
        &self,
        scope: crate::execution::ScopeAbort,
    ) -> Pin<Box<dyn Future<Output = Result<crate::execution::CleanupReceipt, String>> + Send + '_>>
    {
        (**self).abort_scope(scope)
    }
    fn save_execution_checkpoint(
        &self,
        identity: crate::execution::ExecutionIdentity,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        (**self).save_execution_checkpoint(identity, checkpoint)
    }

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

    fn load_pending_approval_checkpoints(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<StoredCheckpoint>, String>> + Send + '_>> {
        (**self).load_pending_approval_checkpoints(session_id)
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
    aborted: Arc<Mutex<std::collections::HashSet<(String, String)>>>,
    notice_receipts: Arc<Mutex<std::collections::HashSet<coda_core::task::TaskId>>>,
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

    fn belongs_to_session(
        checkpoint: &StoredCheckpoint,
        checkpoints: &HashMap<String, StoredCheckpoint>,
        session_id: &str,
    ) -> bool {
        let mut current = checkpoint;
        let mut visited = std::collections::HashSet::new();
        loop {
            if current.thread_id == session_id {
                return true;
            }
            let Some(parent_id) = current.parent_thread_id.as_ref() else {
                return false;
            };
            if parent_id == session_id {
                return true;
            }
            if !visited.insert(parent_id) {
                return false;
            }
            let Some(parent) = checkpoints.get(parent_id) else {
                return false;
            };
            current = parent;
        }
    }
}

impl SessionStorage for MemoryStorage {
    fn has_notice_receipt(
        &self,
        task_id: coda_core::task::TaskId,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + '_>> {
        Box::pin(async move { Ok(self.notice_receipts.lock().await.contains(&task_id)) })
    }
    fn admit_task_notice(
        &self,
        task_id: coda_core::task::TaskId,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let mut checkpoints = self.checkpoints.lock().await;
            let mut receipts = self.notice_receipts.lock().await;
            if receipts.insert(task_id) {
                checkpoints.insert(checkpoint.thread_id.clone(), checkpoint);
            }
            Ok(())
        })
    }

    fn load_background_checkpoints(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<StoredCheckpoint>, String>> + Send + '_>> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            let checkpoints = self.checkpoints.lock().await;
            Ok(checkpoints
                .values()
                .filter(|c| {
                    Self::belongs_to_session(c, &checkpoints, &session_id)
                        && c.active_execution
                            .as_ref()
                            .is_some_and(|e| e.background_task().is_some())
                })
                .cloned()
                .collect())
        })
    }
    fn save_execution_checkpoint(
        &self,
        identity: crate::execution::ExecutionIdentity,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let mut checkpoints = self.checkpoints.lock().await;
            if self
                .aborted
                .lock()
                .await
                .contains(&(identity.thread_id.clone(), identity.invocation_id))
            {
                return Err("execution was aborted".into());
            }
            checkpoints.insert(identity.thread_id, checkpoint);
            Ok(())
        })
    }
    fn abort_scope(
        &self,
        scope: crate::execution::ScopeAbort,
    ) -> Pin<Box<dyn Future<Output = Result<crate::execution::CleanupReceipt, String>> + Send + '_>>
    {
        Box::pin(async move {
            let mut checkpoints = self.checkpoints.lock().await;
            let mut snapshots = self.snapshots.lock().await;
            let mut aborted = self.aborted.lock().await;
            for member in &scope.members {
                aborted.insert((member.thread_id.clone(), member.invocation_id.clone()));
                if let Some(checkpoint) = checkpoints.get_mut(&member.thread_id)
                    && checkpoint
                        .active_execution
                        .as_ref()
                        .is_some_and(|e| e.background_task() == Some(&scope.task_id))
                {
                    crate::execution::abort_checkpoint(checkpoint, &scope.reason);
                }
            }
            for snapshot in snapshots.values_mut() {
                crate::execution::remove_scope_messages(snapshot, &scope.members);
            }
            Ok(crate::execution::CleanupReceipt {
                task_id: scope.task_id,
            })
        })
    }

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

    fn load_pending_approval_checkpoints(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<StoredCheckpoint>, String>> + Send + '_>> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            let stored = self.checkpoints.lock().await;
            let mut checkpoints: Vec<_> = stored
                .values()
                .filter(|checkpoint| {
                    Self::belongs_to_session(checkpoint, &stored, &session_id)
                        && matches!(
                            checkpoint.resume_point,
                            StoredResumePoint::PendingApproval {
                                ref pending_approval_calls,
                                ..
                            } if !pending_approval_calls.is_empty()
                        )
                })
                .cloned()
                .collect();
            checkpoints.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
            Ok(checkpoints)
        })
    }

    fn save_session_snapshot(
        &self,
        session_id: String,
        snapshot: StoredRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let checkpoints = self.checkpoints.lock().await;
            let mut snapshots = self.snapshots.lock().await;
            let aborted: Vec<_> = self
                .aborted
                .lock()
                .await
                .iter()
                .map(|(thread_id, invocation_id)| coda_core::task::ScopeMember {
                    thread_id: thread_id.clone(),
                    invocation_id: invocation_id.clone(),
                })
                .collect();
            let active = checkpoints
                .iter()
                .filter_map(|(thread, checkpoint)| {
                    checkpoint
                        .active_execution
                        .as_ref()
                        .map(|e| (thread.clone(), e.invocation_id.clone()))
                })
                .collect();
            let mut snapshot = snapshot;
            crate::execution::fence_snapshot(&mut snapshot, &aborted, &active);
            snapshots.insert(session_id, snapshot);
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
    /// Thread id → agent name for drivers that exited with unfinished work.
    pub active_threads: HashMap<String, String>,
}

/// A caller's answer to one agent's pending approval, addressed to the thread
/// that is actually parked on it.
///
/// The thread comes from the *checkpoint* that holds the `PendingApproval`, not
/// from the runtime snapshot: the snapshot is written when an agent exits, so it
/// is missing entirely for a session that was killed mid-approval or minted by a
/// fork. Routing a decision through the snapshot dropped it on the floor in
/// exactly those cases, leaving the thread suspended forever.
pub(crate) struct ResumeTarget {
    pub agent_name: String,
    pub thread_id: ThreadId,
    pub decision: ResumeDecision,
}

/// Every envelope [`AgentRuntime::bootstrap`] is about to put back. Within an
/// thread, inbox contents precede messages captured during the final drain;
/// thread ids are sorted to keep recovery validation deterministic.
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

/// The turn a stored thread was last working on.
fn last_turn(checkpoint: &StoredCheckpoint) -> Option<TurnId> {
    checkpoint.messages.last().map(|entry| entry.turn_id)
}

/// Throw away the replayed envelopes their recipient is no longer waiting for.
///
/// Nothing rewrites a snapshot until an agent exits, so a second crash hands
/// back envelopes the first recovery already delivered. The thread drops such
/// an answer on arrival — but only after it has restored the finished turn the
/// answer names, leaving that turn on the books with nothing to end it.
fn drop_stale_envelopes(
    snapshot: &mut AgentRuntimeSnapshot,
    checkpoints: &HashMap<String, StoredCheckpoint>,
) {
    for (name, envelopes) in snapshot
        .agent_drained_envelopes
        .iter_mut()
        .chain(snapshot.drained_envelopes.iter_mut())
    {
        envelopes.retain(|envelope| {
            // Whether the thread this envelope is addressed to is still waiting for it. A
            // thread with no checkpoint at all is not: it has no state to take an answer
            // into.
            //
            // Answers are matched by the envelope that carried the call out: a `call_id`
            // is only unique within one assistant message, so an answer to an earlier
            // invocation that reused it would pass for the one being waited on.
            let awaited = match (
                &envelope.body,
                checkpoints
                    .get(envelope.to.thread_id.as_ref())
                    .map(|checkpoint| &checkpoint.resume_point),
            ) {
                // These carry the work they open, so nothing has to be waiting for them.
                (EnvelopeBody::Task { .. } | EnvelopeBody::ToolCall { .. }, _) => true,
                (EnvelopeBody::Reply { .. }, Some(StoredResumePoint::ToolExecution(state))) => {
                    state.pending_replies.iter().any(|pending| {
                        Some(&pending.call_envelope_id) == envelope.reply_to.as_ref()
                    })
                }
                (EnvelopeBody::Resume(_), Some(StoredResumePoint::PendingApproval { .. })) => true,
                _ => false,
            };
            if !awaited {
                warn!("Dropping an envelope {} already took: {:?}", name, envelope);
            }
            awaited
        });
    }
}

type DriverFactory = Arc<
    dyn Fn(
            AgentRuntime,
            ThreadId,
            Option<ThreadId>,
            Option<ResumeDecision>,
            mpsc::Receiver<AgentControl>,
            mpsc::Receiver<Envelope>,
        ) -> Pin<Box<dyn Future<Output = String> + Send>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub(crate) struct AgentRuntime {
    session_id: String,
    /// Drivers are addressed by thread, including concurrent calls to one agent.
    agents: Arc<Mutex<HashMap<String, AgentHandle>>>,
    agent_tasks: Arc<std::sync::Mutex<JoinSet<String>>>,
    driver_factories: Arc<HashMap<String, DriverFactory>>,
    wait_gate: Arc<Mutex<()>>,
    /// Global event bus — all agents forward their events here.
    global_event_tx: broadcast::Sender<(String, ThreadId, TurnId, AgentEvent)>,
    pub(crate) session_storage: Arc<dyn SessionStorage>,
    exit_barrier: ExitBarrier,
    snapshot: Arc<Mutex<AgentRuntimeSnapshot>>,
    turn_gate: Arc<TurnGate>,
    calls: Arc<CallLedger>,
    pub(crate) background: Option<Arc<coda_process::BackgroundTasks>>,
    executions: Arc<std::sync::Mutex<scopes::Executions>>,
    approval_status_gate: Arc<Mutex<()>>,
    root_state: Arc<std::sync::Mutex<Option<Arc<Mutex<crate::agent::AgentState>>>>>,
}

impl AgentRuntime {
    pub(crate) fn register_root_state(
        &self,
        thread: &ThreadId,
        state: Arc<Mutex<crate::agent::AgentState>>,
    ) {
        if self.is_root_thread(thread) {
            *self.root_state.lock().expect("root state") = Some(state);
        }
    }
    pub(crate) async fn live_messages(&self) -> Result<Vec<coda_core::llm::Message>, String> {
        let state = self.root_state.lock().expect("root state").clone();
        if let Some(state) = state {
            return Ok(state.lock().await.messages());
        }
        Ok(self
            .session_storage
            .load_checkpoint(&self.session_id)
            .await?
            .map(|c| c.messages.into_iter().map(|e| e.message).collect())
            .unwrap_or_default())
    }

    pub(crate) fn new(session_storage: impl SessionStorage + 'static, session_id: String) -> Self {
        // Sized for chunk-level event bursts (LLM streaming): a lagged receiver
        // drops events, which consumers can only partially recover from. A
        // margin against scheduling delays, not a response to an observed failure.
        let (global_event_tx, _) = broadcast::channel(256);
        AgentRuntime {
            session_id,
            agents: Arc::new(Mutex::new(HashMap::new())),
            agent_tasks: Arc::new(std::sync::Mutex::new(JoinSet::new())),
            driver_factories: Arc::new(HashMap::new()),
            wait_gate: Arc::new(Mutex::new(())),
            global_event_tx,
            session_storage: Arc::new(session_storage),
            exit_barrier: ExitBarrier::default(),
            snapshot: Arc::new(Mutex::new(AgentRuntimeSnapshot::default())),
            turn_gate: Arc::new(TurnGate::default()),
            calls: Arc::new(CallLedger::default()),
            background: None,
            approval_status_gate: Arc::new(Mutex::new(())),
            root_state: Arc::new(std::sync::Mutex::new(None)),
            executions: Arc::new(std::sync::Mutex::new(scopes::Executions::default())),
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
        self.request_abort().await;
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
        last_turn(&self.checkpoint_of(thread_id).await?)
    }

    /// The stored checkpoints recovery has to consult, loaded once each — one
    /// thread can be named by the active-thread map and by several envelopes at
    /// once. Only the recipients of answers are consulted: a `Task` or
    /// `ToolCall` is judged by what it carries, not by the state it lands in.
    ///
    /// A read that fails gives up the whole recovery, because every decision
    /// below is made by reading these: guessing would either throw away an
    /// undelivered answer or restore a turn nothing will finish.
    async fn recovery_checkpoints(
        &self,
        snapshot: &AgentRuntimeSnapshot,
    ) -> Result<HashMap<String, StoredCheckpoint>, String> {
        let mut consulted: Vec<String> = snapshot
            .active_threads
            .keys()
            .cloned()
            .chain(
                replayed_envelopes(snapshot)
                    .into_iter()
                    .filter(|envelope| {
                        matches!(
                            envelope.body,
                            EnvelopeBody::Reply { .. } | EnvelopeBody::Resume(_)
                        )
                    })
                    .map(|envelope| envelope.to.thread_id.as_ref().to_string()),
            )
            .collect();
        consulted.sort_unstable();
        consulted.dedup();

        let mut checkpoints = HashMap::with_capacity(consulted.len());
        for thread_id in consulted {
            let loaded = self
                .session_storage
                .load_checkpoint(&thread_id)
                .await
                .map_err(|err| format!("failed to load checkpoint for {thread_id}: {err}"))?;
            if let Some(checkpoint) = loaded {
                checkpoints.insert(thread_id, checkpoint);
            }
        }
        Ok(checkpoints)
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
    fn register_resumed_work(
        &self,
        snapshot: &AgentRuntimeSnapshot,
        checkpoints: &HashMap<String, StoredCheckpoint>,
    ) -> Result<(), String> {
        // Active threads and replayed envelopes are independent evidence for
        // the same turn; `TurnGate::restore` verifies that they agree.
        for thread_id in snapshot.active_threads.keys() {
            let Some(checkpoint) = checkpoints.get(thread_id) else {
                continue;
            };
            if let Some(turn) = last_turn(checkpoint) {
                self.turn_gate.restore(turn)?;
            }
            // A stored reply target outlives only an unanswered call: it is
            // taken before the checkpoint that precedes the reply, so a thread
            // that already answered has none.
            if checkpoint
                .active_execution
                .as_ref()
                .and_then(|e| e.reply_target())
                .is_some()
            {
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
                    if let Some(turn) = checkpoints
                        .get(envelope.to.thread_id.as_ref())
                        .and_then(last_turn)
                    {
                        self.turn_gate.restore(turn)?;
                    }
                    // An answer nobody has taken yet still settles an obligation
                    // when its caller does take it.
                    if let Sender::Agent { thread_id, .. } = &envelope.from {
                        self.calls.begin(thread_id);
                    }
                }
                EnvelopeBody::Resume(_) => {
                    if let Some(turn) = checkpoints
                        .get(envelope.to.thread_id.as_ref())
                        .and_then(last_turn)
                    {
                        self.turn_gate.restore(turn)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Put back the turn and outstanding reply each decision-routed resume is
    /// about to continue.
    ///
    /// [`Self::register_resumed_work`] sees only threads the snapshot named, and
    /// the sessions this routing exists for — killed mid-approval, or minted by
    /// a fork — have no snapshot naming them. Left out of the active slot, the
    /// resumed work would run outside single-flight: a task submitted alongside
    /// it would open a second turn, and an abort would find nothing to mark.
    ///
    /// Bootstrap removes these agents from `snapshot.active_threads` first, so
    /// this path also restores the reply obligation that snapshot recovery
    /// would otherwise have registered for a sub-agent.
    async fn restore_resumed_turns(
        &self,
        resume_targets: &HashMap<String, ResumeTarget>,
    ) -> Result<(), String> {
        for target in resume_targets.values() {
            let thread_id = target.thread_id.as_ref();
            let checkpoint = self
                .session_storage
                .load_checkpoint(thread_id)
                .await
                .map_err(|err| format!("failed to load checkpoint for {thread_id}: {err}"))?
                .ok_or_else(|| format!("missing checkpoint for resumed thread {thread_id}"))?;
            if let Some(turn) = last_turn(&checkpoint) {
                self.turn_gate.restore(turn)?;
            }
            if checkpoint
                .active_execution
                .as_ref()
                .and_then(|e| e.reply_target())
                .is_some()
            {
                self.calls.begin(&target.thread_id);
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
        let task = self
            .execution(&thread_id)
            .and_then(|e| e.background_task().cloned());
        if let AgentEvent::Suspended(approval) = &event {
            let mut state = self.executions.lock().expect("executions");
            if task
                .as_ref()
                .is_some_and(|id| state.background.get(id).is_some_and(|s| s.closed))
            {
                return;
            }
            state.approvals.insert(
                (approval.thread_id.clone(), approval.parent_message_id),
                approval.clone(),
            );
        }
        if matches!(&event, AgentEvent::Aborted(_) | AgentEvent::Error(_))
            || matches!(&event, AgentEvent::LLMEnd(answer) if answer.tool_calls.is_empty())
        {
            let removed: Vec<_> = {
                let mut state = self.executions.lock().expect("executions");
                let keys: Vec<_> = state
                    .approvals
                    .keys()
                    .filter(|(thread, _)| thread == thread_id.as_ref())
                    .cloned()
                    .collect();
                keys.into_iter()
                    .filter_map(|key| state.approvals.remove(&key))
                    .collect()
            };
            for approval in removed {
                let _ = self.global_event_tx.send((
                    agent_name.clone(),
                    thread_id.clone(),
                    turn_id,
                    AgentEvent::ApprovalRemoved {
                        thread_id: approval.thread_id,
                        parent_message_id: approval.parent_message_id,
                        task_id: approval.task_id,
                    },
                ));
            }
        }
        if let Some(task_id) = &task {
            match &event {
                AgentEvent::Suspended(_) => {
                    self.refresh_approval_status(task_id).await;
                }
                AgentEvent::PersistFailed(message) => {
                    let _ = self.global_event_tx.send((
                        agent_name,
                        thread_id,
                        turn_id,
                        AgentEvent::BackgroundError {
                            task_id: task_id.clone(),
                            message: message.clone(),
                        },
                    ));
                    return;
                }
                AgentEvent::ApprovalRemoved { .. } | AgentEvent::BackgroundError { .. } => {}
                _ => return,
            }
        }
        let _ = self
            .global_event_tx
            .send((agent_name, thread_id, turn_id, event));
    }

    /// Start the agents, putting back whatever the snapshot left mid-flight and
    /// whatever the caller's resume decisions answer.
    ///
    /// Reports whether any of it will actually run — a thread to resume or an
    /// envelope to replay. Only known here, once recovery has thrown out what
    /// nothing will pick up again.
    pub(crate) async fn bootstrap(
        &mut self,
        agents: HashMap<String, Agent>,
        mut snapshot: Option<AgentRuntimeSnapshot>,
        mut resume_targets: HashMap<String, ResumeTarget>,
        config: RunConfig<impl LLMProvider + Clone>,
    ) -> Result<bool, String> {
        self.driver_factories = Arc::new(
            agents
                .into_iter()
                .map(|(name, agent)| {
                    let config = config.resolve(&name);
                    let factory: DriverFactory = Arc::new(
                        move |runtime, thread_id, active, decision, control, inbox| {
                            let agent = agent.for_thread();
                            let config = config.clone();
                            Box::pin(async move {
                                driver::run_agent(
                                    runtime,
                                    thread_id.clone(),
                                    (active, decision),
                                    agent,
                                    control,
                                    inbox,
                                    config,
                                )
                                .await;
                                thread_id.0
                            })
                        },
                    );
                    (name, factory)
                })
                .collect(),
        );
        resume_targets.retain(|_, target| self.driver_factories.contains_key(&target.agent_name));
        let mut resuming = !resume_targets.is_empty();
        if let Some(snapshot) = snapshot.as_mut() {
            snapshot
                .active_threads
                .retain(|_, name| self.driver_factories.contains_key(name));
            for envelopes in snapshot
                .agent_drained_envelopes
                .values_mut()
                .chain(snapshot.drained_envelopes.values_mut())
            {
                envelopes.retain(|envelope| self.driver_factories.contains_key(&envelope.to.name));
            }
            for target in resume_targets.values() {
                snapshot.active_threads.remove(target.thread_id.as_ref());
            }
            let checkpoints = self.recovery_checkpoints(snapshot).await?;
            if checkpoints.values().any(|checkpoint| {
                checkpoint
                    .active_execution
                    .as_ref()
                    .is_some_and(|execution| execution.background_task().is_some())
            }) {
                return Err("background execution must be aborted before recovery".into());
            }
            drop_stale_envelopes(snapshot, &checkpoints);
            self.register_resumed_work(snapshot, &checkpoints)?;
            resuming |=
                !snapshot.active_threads.is_empty() || !replayed_envelopes(snapshot).is_empty();
        }
        self.restore_resumed_turns(&resume_targets).await?;
        let snapshot = snapshot.unwrap_or_default();
        let mut threads = snapshot.active_threads.clone();
        let mut inboxes: HashMap<String, Vec<Envelope>> = HashMap::new();
        for envelope in replayed_envelopes(&snapshot) {
            let thread = envelope.to.thread_id.as_ref().to_owned();
            threads.insert(thread.clone(), envelope.to.name.clone());
            inboxes.entry(thread).or_default().push(envelope.clone());
        }
        for target in resume_targets.values() {
            threads.insert(target.thread_id.0.clone(), target.agent_name.clone());
        }
        for (thread, name) in threads {
            let target = resume_targets.remove(&thread);
            let active = (target.is_some() || snapshot.active_threads.contains_key(&thread))
                .then(|| ThreadId::from(thread.clone()));
            let envelopes = inboxes.remove(&thread).unwrap_or_default();
            self.start_driver(
                &name,
                ThreadId::from(thread),
                active,
                target.map(|t| t.decision),
                envelopes,
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        Ok(resuming)
    }

    /// Subscribe to events from all agents
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<(String, ThreadId, TurnId, AgentEvent)> {
        self.global_event_tx.subscribe()
    }

    async fn broadcast_command(&self, cmd: AgentControl) {
        let agents: Vec<_> = self.agents.lock().await.values().cloned().collect();
        for entry in agents {
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
        let threads: Vec<_> = {
            let state = self.executions.lock().expect("executions");
            state
                .threads
                .iter()
                .filter(|(_, e)| {
                    matches!(
                        e.stored.scope,
                        crate::execution::ExecutionScope::Foreground { .. }
                    )
                })
                .map(|(thread, e)| {
                    e.cancel.cancel();
                    thread.clone()
                })
                .collect()
        };
        let handles: Vec<_> = {
            let drivers = self.agents.lock().await;
            threads
                .iter()
                .filter_map(|thread| drivers.get(thread).cloned())
                .collect()
        };
        for handle in handles {
            let _ = handle.send_command(AgentControl::Abort).await;
        }
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
        if let Some(thread_id) = &dispatched
            && !self.calls.try_begin(thread_id)
        {
            return Err(SendCommandError::ThreadBusy);
        }
        if let EnvelopeBody::Resume(decision) = &envelope.body {
            let removed = {
                let mut state = self.executions.lock().expect("executions");
                let key = (envelope.to.thread_id.0.clone(), decision.parent_message_id);
                let approval = state
                    .approvals
                    .get(&key)
                    .ok_or(SendCommandError::StaleApproval)?;
                let mut ids = std::collections::HashSet::new();
                if approval.agent_name != envelope.to.name
                    || decision.resolutions.iter().any(|(id, _)| {
                        !ids.insert(id) || !approval.calls.iter().any(|call| &call.id == id)
                    })
                {
                    return Err(SendCommandError::StaleApproval);
                }
                if state
                    .threads
                    .get(envelope.to.thread_id.as_ref())
                    .is_some_and(|e| e.cancel.is_cancelled())
                {
                    return Err(SendCommandError::ScopeClosed);
                }
                state.approvals.remove(&key)
            };
            if let Some(approval) = removed {
                if let Some(id) = &approval.task_id {
                    self.refresh_approval_status(id).await;
                }
                let turn = self
                    .turn_of_thread(&approval.thread_id)
                    .await
                    .unwrap_or_else(|| TurnId::from(decision.parent_message_id));
                self.emit_event(
                    approval.agent_name,
                    envelope.to.thread_id.clone(),
                    turn,
                    AgentEvent::ApprovalRemoved {
                        thread_id: approval.thread_id,
                        parent_message_id: approval.parent_message_id,
                        task_id: approval.task_id,
                    },
                )
                .await;
            }
        }
        let sent = match self.register_execution(&envelope) {
            Ok(()) => {
                if matches!(envelope.body, EnvelopeBody::ToolCall { .. })
                    && let Some(execution) = self.execution(&envelope.to.thread_id)
                    && let Some(id) = execution.background_task()
                    && let Err(error) = self.persist_scope_members(id).await
                {
                    self.checkpoint_failed(
                        crate::execution::ExecutionIdentity {
                            thread_id: envelope.to.thread_id.0.clone(),
                            invocation_id: execution.invocation_id.clone(),
                        },
                        error,
                    );
                    return Err(SendCommandError::ScopeClosed);
                }
                self.deliver(envelope).await
            }
            Err(error) => Err(error),
        };
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

    async fn retire_foreground_driver(&self, agent_name: &str, thread_id: &ThreadId) {
        self.save_agent_snapshot(agent_name.into(), thread_id.clone(), vec![], None)
            .await;
        self.agents.lock().await.remove(thread_id.as_ref());
        self.executions
            .lock()
            .expect("executions")
            .threads
            .remove(thread_id.as_ref());
    }

    async fn start_driver(
        &self,
        name: &str,
        thread_id: ThreadId,
        active: Option<ThreadId>,
        decision: Option<ResumeDecision>,
        envelopes: Vec<Envelope>,
    ) -> Result<AgentHandle, SendCommandError> {
        {
            let mut tasks = self.agent_tasks.lock().expect("driver tasks");
            while let Some(result) = tasks.try_join_next() {
                if let Err(error) = result {
                    warn!(%error, "Agent task failed to join");
                }
            }
        }
        let mut handles = self.agents.lock().await;
        if let Some(handle) = handles.get(thread_id.as_ref()) {
            return Ok(handle.clone());
        }
        let factory = self
            .driver_factories
            .get(name)
            .ok_or(SendCommandError::AgentNotFound)?;
        let (control_sender, control) = mpsc::channel(8);
        let (message_sender, inbox) = mpsc::channel(8 + envelopes.len());
        for envelope in envelopes {
            message_sender
                .try_send(envelope)
                .expect("initial inbox has reserved capacity");
        }
        let (finished_tx, finished) = tokio::sync::watch::channel(false);
        let work = factory(
            self.clone(),
            thread_id.clone(),
            active,
            decision,
            control,
            inbox,
        );
        let completion = DriverFinished(finished_tx);
        let abort = self
            .agent_tasks
            .lock()
            .expect("driver tasks")
            .spawn(async move {
                let _completion = completion;
                work.await
            });
        let handle = AgentHandle {
            control_sender,
            message_sender,
            abort,
            finished,
        };
        handles.insert(thread_id.0.clone(), handle.clone());
        Ok(handle)
    }

    async fn deliver(&self, envelope: Envelope) -> Result<(), SendCommandError> {
        if self.exit_barrier.is_exiting() {
            let receiver = envelope.to.thread_id.0.clone();
            let mut snapshot = self.snapshot.lock().await;
            snapshot
                .drained_envelopes
                .entry(receiver)
                .or_default()
                .push(envelope);
            if let Err(err) = self
                .session_storage
                .save_session_snapshot(self.session_id.clone(), snapshot.clone().into())
                .await
            {
                warn!("Failed to persist session snapshot on buffered message: {err}");
            }
            return Ok(());
        }
        let handle = self
            .start_driver(
                &envelope.to.name,
                envelope.to.thread_id.clone(),
                None,
                None,
                vec![],
            )
            .await?;
        handle.send_message(envelope).await
    }

    pub(crate) async fn save_agent_snapshot(
        &self,
        agent_name: String,
        driver_thread: ThreadId,
        envelopes: Vec<Envelope>,
        active_thread: Option<ThreadId>,
    ) {
        if self
            .execution(&driver_thread)
            .is_some_and(|e| e.background_task().is_some())
            && self.execution_stopped(&driver_thread)
        {
            return;
        }
        let mut snapshot = self.snapshot.lock().await;
        if envelopes.is_empty() {
            snapshot
                .agent_drained_envelopes
                .remove(driver_thread.as_ref());
        } else {
            snapshot
                .agent_drained_envelopes
                .insert(driver_thread.0.clone(), envelopes);
        }
        match active_thread {
            Some(thread_id) => {
                snapshot.active_threads.insert(thread_id.0, agent_name);
            }
            None => {
                snapshot.active_threads.remove(driver_thread.as_ref());
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
        let _wait = self.wait_gate.lock().await;
        if self.agent_tasks.lock().expect("driver tasks").is_empty() {
            return true;
        }
        if timeout(duration, Self::drain(&self.agent_tasks))
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
        let _wait = self.wait_gate.lock().await;
        if self.agent_tasks.lock().expect("driver tasks").is_empty() {
            return true;
        }

        let ret = match timeout_duration {
            Some(duration) => timeout(duration, Self::drain(&self.agent_tasks))
                .await
                .is_ok(),
            None => {
                Self::drain(&self.agent_tasks).await;
                true
            }
        };
        if !ret {
            warn!("aborting agent tasks that outstayed the shutdown deadline");
            self.agent_tasks.lock().expect("driver tasks").abort_all();
            Self::drain(&self.agent_tasks).await;
        }
        self.persist_snapshot(timeout_duration).await;

        ret
    }

    async fn drain(agent_tasks: &std::sync::Mutex<JoinSet<String>>) {
        while let Some(result) =
            std::future::poll_fn(|cx| agent_tasks.lock().expect("driver tasks").poll_join_next(cx))
                .await
        {
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

struct DriverFinished(tokio::sync::watch::Sender<bool>);
impl Drop for DriverFinished {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
