//! Process-level session relay: live agent [`Session`]s live here, decoupled
//! from the WebSocket connections that drive them.
//!
//! Connections *attach* to a session (latest-wins: attaching evicts the
//! previous client) and receive a snapshot plus a single ordered event stream
//! that replays the in-flight turn before switching to live events. A
//! disconnect merely detaches; a running turn keeps going and the session is
//! released (gracefully, checkpoint on disk) once it is idle *and* unattached.
//!
//! [`SessionRelay`] is the only abstraction the connection layer sees. All its
//! inputs/outputs are plain data (no closures), so a future multi-instance
//! implementation can forward commands to the owning instance and tail events
//! from a shared log (e.g. Redis Streams) without touching the callers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::sync::Arc;

use coda_agent::{
    AgentEvent, OpenError, PendingApproval, ResumeDecision, Session, SessionStreamItem, Shutdown,
};
use coda_core::llm::{Message, ReasoningEffort, UserMessage};
use futures::StreamExt as _;
use futures::stream::BoxStream;
use tokio::sync::{Mutex, OwnedMutexGuard, mpsc, watch};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{error, info, warn};

use crate::wire::WireEvent;

pub type SessionKey = (String, String); // (workspace_id, session_id)
pub type ConnId = u64;

/// A command a client issues against an attached session. Plain data only —
/// see the module docs for why.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    Task {
        task: String,
        images: Vec<String>,
    },
    Resume {
        agent_name: String,
        thread_id: String,
        decision: ResumeDecision,
    },
    Abort,
    SetModel {
        provider_id: String,
        reasoning_effort: Option<ReasoningEffort>,
    },
}

/// An element of the per-attachment event stream.
#[derive(Debug)]
pub enum RelayEvent {
    Event(Box<WireEvent>),
    /// Another client attached to this session; this stream ends after this.
    Evicted,
    /// The session runtime ended (released, deleted, or replaced); this stream
    /// ends after this.
    Closed,
}

/// What a client needs to render a session at attach time.
#[derive(Debug, Clone)]
pub struct SnapshotPayload {
    pub messages: Vec<Message>,
    pub pending_approvals: Vec<PendingApproval>,
    pub provider_id: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// A turn is in flight; its events so far are replayed at the head of the
    /// attach stream.
    pub turn_running: bool,
}

pub struct AttachSession {
    pub snapshot: SnapshotPayload,
    /// Replay of the current turn followed seamlessly by live events. Ends
    /// after [`RelayEvent::Evicted`] / [`RelayEvent::Closed`], or silently on
    /// detach/release.
    pub events: BoxStream<'static, RelayEvent>,
}

/// Result of [`SessionRelay::command`], driving the connection layer's
/// client-facing responses.
pub enum CommandOutcome {
    /// The command was accepted (or was a benign no-op).
    Ok,
    /// Stale connection or invalid state; logged, nothing to send.
    Ignored,
    /// A `Resume` against an approvals-gated open that still needs more
    /// decisions; the client should be shown these approvals.
    StillPending(Vec<PendingApproval>),
    /// A `SetModel` was applied.
    ModelChanged {
        provider_id: String,
        reasoning_effort: Option<ReasoningEffort>,
    },
    /// Opening the session failed (approvals-gated promotion or `SetModel`).
    OpenFailed(OpenError),
}

/// Builds sessions for the relay. Injected at construction: configuration is
/// available on every instance, so commands never need to carry build logic.
pub trait SessionOpener: Send + Sync + 'static {
    /// Open (or resume) the session for `key`, seeded with `decisions` for any
    /// pending approvals carried over from a prior suspension.
    fn open<'a>(
        &'a self,
        key: &'a SessionKey,
        provider_id: &'a str,
        reasoning_effort: Option<ReasoningEffort>,
        decisions: HashMap<String, ResumeDecision>,
    ) -> Pin<Box<dyn Future<Output = Result<Session, OpenError>> + Send + 'a>>;

    /// Load the persisted conversation history for `key` (empty when none).
    /// Used for the snapshot of approvals-gated opens, where no live session
    /// exists yet.
    fn load_messages<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Vec<Message>> + Send + 'a>>;
}

/// The connection layer's only interface to sessions. See the module docs.
pub trait SessionRelay: Send + Sync {
    /// Open-or-attach (latest-wins): evicts any client currently attached to
    /// `key`. `provider_id`/`reasoning_effort` must be pre-validated against
    /// the provider catalog; they only apply when the session is not already
    /// live.
    fn attach<'a>(
        &'a self,
        key: SessionKey,
        conn_id: ConnId,
        provider_id: String,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Pin<Box<dyn Future<Output = Result<AttachSession, OpenError>> + Send + 'a>>;

    /// Drive an attached session. Rejected (with a warn) when `conn_id` is not
    /// the currently attached client.
    fn command<'a>(
        &'a self,
        key: SessionKey,
        conn_id: ConnId,
        command: SessionCommand,
    ) -> Pin<Box<dyn Future<Output = CommandOutcome> + Send + 'a>>;

    /// Release this connection's claim on `key` (CloseSession). The session
    /// keeps running while a turn is in flight and is released once idle.
    fn detach<'a>(
        &'a self,
        key: SessionKey,
        conn_id: ConnId,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Release all of `conn_id`'s claims (connection closed).
    fn detach_all<'a>(&'a self, conn_id: ConnId) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Stop and remove the session immediately (aborting in-flight work, no
    /// checkpoint write-back). Returns `true` once the runtime is gone so the
    /// caller can safely delete persisted state, and `false` when the session
    /// is currently attached by a *different* connection — a stale client must
    /// not be able to erase work another client is driving (latest-wins).
    /// Unattached sessions (e.g. deleting history from the catalog) are fair
    /// game for any connection.
    fn delete<'a>(
        &'a self,
        key: SessionKey,
        conn_id: ConnId,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

    /// The provider a live (or pending) session was opened with.
    fn provider_of<'a>(
        &'a self,
        key: SessionKey,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

    /// Gracefully stop every session (process shutdown).
    fn shutdown_all<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// True when `event` ends the current turn: the root agent's final `LlmEnd`
/// (no tool calls, not aborted — an aborted partial message is always followed
/// by the `Aborted` marker, which is the single settle signal for that path),
/// any suspension, or the root agent aborting/erroring.
pub fn event_settles_turn(event: &WireEvent, root_name: &str) -> bool {
    match event {
        WireEvent::LlmEnd {
            agent_name,
            message,
            ..
        } => agent_name == root_name && message.tool_calls.is_empty() && !message.aborted,
        WireEvent::Suspended { .. } => true,
        WireEvent::Aborted { agent_name, .. } | WireEvent::Error { agent_name, .. } => {
            agent_name == root_name
        }
        _ => false,
    }
}

/// Soft cap on buffered events for one turn. On overflow the oldest chunk-tier
/// event is dropped first; message-bearing events are never dropped so the
/// settle-fold cannot lose history.
const MAX_LOG_EVENTS: usize = 8192;

/// The current turn's events, in order. Cleared when the turn settles (the
/// settled turn is folded into the entry's snapshot instead).
#[derive(Default)]
struct EventLog {
    entries: VecDeque<WireEvent>,
    overflowed: bool,
}

fn is_chunk_tier(event: &WireEvent) -> bool {
    matches!(
        event,
        WireEvent::LlmStart { .. }
            | WireEvent::LlmContentChunk { .. }
            | WireEvent::LlmReasoningChunk { .. }
            | WireEvent::ToolCallStart { .. }
    )
}

impl EventLog {
    fn push(&mut self, event: WireEvent) {
        // On overflow evict the oldest chunk-tier event; when the log is all
        // message-tier, let it grow — losing one would corrupt the fold.
        if self.entries.len() >= MAX_LOG_EVENTS
            && let Some(pos) = self.entries.iter().position(is_chunk_tier)
        {
            self.entries.remove(pos);
            if !self.overflowed {
                self.overflowed = true;
                warn!(
                    "event log overflowed {MAX_LOG_EVENTS} events; \
                     dropping oldest chunk-tier events (replay will have gaps)"
                );
            }
        }
        self.entries.push_back(event);
    }

    fn iter(&self) -> impl Iterator<Item = &WireEvent> {
        self.entries.iter()
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.overflowed = false;
    }
}

/// Fold the settled turn into `snapshot`, mirroring exactly what the driver
/// appended to the agent's history:
///
/// 1. Leading root `ToolCallEnd`s — stale-envelope cleanups or resume
///    resolutions, which the driver writes *before* the user message.
/// 2. The turn's user prompt (front of `unsettled_users`; absent for resumed
///    turns).
/// 3. The remaining root `LlmEnd`/`ToolCallEnd` messages, in order.
///
/// Sub-agent events and chunk-tier events are skipped (matching what the
/// checkpoint history holds). The log is cleared afterwards.
fn fold_settled_turn(
    snapshot: &mut Vec<Message>,
    unsettled_users: &mut VecDeque<Message>,
    log: &mut EventLog,
    root_name: &str,
) {
    let mut entries = log.iter().peekable();
    while let Some(WireEvent::ToolCallEnd {
        agent_name,
        message,
        ..
    }) = entries.peek()
    {
        if agent_name != root_name {
            break;
        }
        snapshot.push(Message::Tool(message.clone()));
        entries.next();
    }
    if let Some(user) = unsettled_users.pop_front() {
        snapshot.push(user);
    }
    for event in entries {
        match event {
            WireEvent::LlmEnd {
                agent_name,
                message,
                ..
            } if agent_name == root_name => snapshot.push(Message::Assistant(message.clone())),
            WireEvent::ToolCallEnd {
                agent_name,
                message,
                ..
            } if agent_name == root_name => snapshot.push(Message::Tool(message.clone())),
            _ => {}
        }
    }
    log.clear();
}

struct Attachment {
    conn_id: ConnId,
    tx: mpsc::UnboundedSender<RelayEvent>,
}

struct LiveState {
    session: Session,
    provider_id: String,
    reasoning_effort: Option<ReasoningEffort>,
    /// Bumped when `SetModel` swaps the underlying session so the previous
    /// forwarder retires itself.
    generation: u64,
    turn_running: bool,
    /// The settled conversation history, kept in memory. Authoritative for
    /// attach snapshots: the driver's final checkpoint lands *after* the settle
    /// event, so re-reading disk mid-life would race — disk is only read when
    /// an entry is created.
    snapshot: Vec<Message>,
    /// User prompts of turns that have not settled (and thus not folded) yet.
    unsettled_users: VecDeque<Message>,
    pending_approvals: Vec<PendingApproval>,
    log: EventLog,
}

struct PendingState {
    provider_id: String,
    reasoning_effort: Option<ReasoningEffort>,
    /// Thread ids still awaiting a resume decision.
    needed: HashSet<String>,
    decisions: HashMap<String, ResumeDecision>,
    approvals: Vec<PendingApproval>,
    /// History loaded from the persisted checkpoint at entry creation.
    snapshot: Vec<Message>,
}

enum EntryPhase {
    /// Freshly inserted; the creating attach initializes it under the entry
    /// lock (which is what serializes concurrent opens of the same key).
    Uninitialized,
    Live(LiveState),
    /// Approvals-gated open: no runtime yet, resume decisions being collected.
    Pending(PendingState),
    /// Shutdown in progress outside the lock; `done` flips true after the
    /// entry is removed from the map. `watch` carries the value, so a waiter
    /// that subscribes after completion still observes it (no missed wakeup).
    Releasing {
        done: watch::Receiver<bool>,
    },
    /// Tombstone: this Arc is (about to be) gone from the map; retry there.
    Released,
}

struct EntryState {
    phase: EntryPhase,
    /// The single attached client — the latest-wins slot.
    attached: Option<Attachment>,
}

struct SessionEntry {
    key: SessionKey,
    inner: Arc<Mutex<EntryState>>,
}

type EntryGuard = OwnedMutexGuard<EntryState>;
type Entries = Arc<std::sync::Mutex<HashMap<SessionKey, Arc<SessionEntry>>>>;

/// In-process [`SessionRelay`] implementation.
pub struct SessionHub {
    opener: Arc<dyn SessionOpener>,
    entries: Entries,
}

impl SessionHub {
    pub fn new(opener: Arc<dyn SessionOpener>) -> Self {
        Self {
            opener,
            entries: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Get or insert the entry for `key`, waiting out any in-flight release.
    /// Returns with the entry lock held and the phase not `Releasing`/`Released`.
    async fn lock_entry_for_attach(&self, key: &SessionKey) -> (Arc<SessionEntry>, EntryGuard) {
        loop {
            let entry = {
                let mut map = self.entries.lock().expect("entries mutex poisoned");
                map.entry(key.clone())
                    .or_insert_with(|| {
                        Arc::new(SessionEntry {
                            key: key.clone(),
                            inner: Arc::new(Mutex::new(EntryState {
                                phase: EntryPhase::Uninitialized,
                                attached: None,
                            })),
                        })
                    })
                    .clone()
            };
            let guard = entry.inner.clone().lock_owned().await;
            match &guard.phase {
                EntryPhase::Releasing { done } => {
                    let mut done = done.clone();
                    drop(guard);
                    // watch carries the value: a release that completed before
                    // we subscribed is still observed (no missed wakeup).
                    while !*done.borrow_and_update() {
                        if done.changed().await.is_err() {
                            break; // sender dropped == release finished
                        }
                    }
                }
                EntryPhase::Released => {
                    drop(guard);
                    // Tombstone from a raced release; the map slot is (being)
                    // cleared — loop and take the fresh slot.
                    tokio::task::yield_now().await;
                }
                _ => return (entry, guard),
            }
        }
    }

    /// Look up an existing entry without creating one, lock it, and check the
    /// caller is the attached client. `None` for missing/releasing entries or
    /// a stale connection.
    async fn lock_entry_for_conn(
        &self,
        key: &SessionKey,
        conn_id: ConnId,
    ) -> Option<(Arc<SessionEntry>, EntryGuard)> {
        let entry = self
            .entries
            .lock()
            .expect("entries mutex poisoned")
            .get(key)
            .cloned()?;
        let guard = entry.inner.clone().lock_owned().await;
        match guard.attached.as_ref().map(|attachment| attachment.conn_id) {
            Some(attached_conn) if attached_conn == conn_id => Some((entry, guard)),
            _ => {
                warn!(
                    workspace_id = %key.0,
                    session_id = %key.1,
                    "rejecting command from a connection that is not attached"
                );
                None
            }
        }
    }

    /// Transition the entry to `Releasing` and return the work to run *outside*
    /// the entry lock: shut the session down (when there is one), remove the
    /// entry from the map, tombstone it, and signal waiters. `notify_closed`
    /// sends [`RelayEvent::Closed`] to an attached client before its stream
    /// ends (pass `false` when the client initiated the teardown itself).
    fn begin_release(
        entries: &Entries,
        entry: &Arc<SessionEntry>,
        state: &mut EntryState,
        mode: Shutdown,
        notify_closed: bool,
    ) -> impl Future<Output = ()> + Send + 'static {
        let (done_tx, done_rx) = watch::channel(false);
        let phase = std::mem::replace(&mut state.phase, EntryPhase::Releasing { done: done_rx });
        let session = match phase {
            EntryPhase::Live(live) => Some(live.session),
            _ => None,
        };
        // `Closed` is sent before the shutdown below completes; that cannot
        // race a reattach past the checkpoint barrier, because an attach that
        // arrives while the phase is `Releasing` waits for `done` (set only
        // after shutdown returned and the map entry is gone).
        if let Some(attachment) = state.attached.take()
            && notify_closed
        {
            let _ = attachment.tx.send(RelayEvent::Closed);
        }
        let entries = entries.clone();
        let entry = entry.clone();
        async move {
            if let Some(session) = session {
                // `graceful_unbounded` cannot time out and `Abort` waits for
                // full shutdown, so this returning is the durability barrier:
                // the final checkpoint (if any) is on disk before the key can
                // be reopened.
                session.shutdown(mode).await;
            }
            {
                let mut map = entries.lock().expect("entries mutex poisoned");
                if map
                    .get(&entry.key)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    map.remove(&entry.key);
                }
            }
            entry.inner.lock().await.phase = EntryPhase::Released;
            let _ = done_tx.send(true);
            info!(workspace_id = %entry.key.0, session_id = %entry.key.1, "session released");
        }
    }

    /// Release the entry when nothing keeps it alive: no attached client and
    /// no running turn. Returns the outside-the-lock work, if any.
    fn maybe_release(
        entries: &Entries,
        entry: &Arc<SessionEntry>,
        state: &mut EntryState,
    ) -> Option<impl Future<Output = ()> + Send + 'static> {
        if state.attached.is_some() {
            return None;
        }
        let idle = match &state.phase {
            EntryPhase::Live(live) => !live.turn_running,
            EntryPhase::Pending(_) => true,
            _ => false,
        };
        idle.then(|| {
            Self::begin_release(entries, entry, state, Shutdown::graceful_unbounded(), false)
        })
    }

    /// Build a `LiveState` around a freshly opened session and start its event
    /// pipeline (pump + forwarder).
    fn make_live(
        &self,
        entry: &Arc<SessionEntry>,
        session: Session,
        provider_id: String,
        reasoning_effort: Option<ReasoningEffort>,
        generation: u64,
    ) -> LiveState {
        let root_name = session.root_name().to_string();
        let snapshot = session
            .resumed_messages()
            .map(<[Message]>::to_vec)
            .unwrap_or_default();
        let turn_running = session.has_resuming_agents();
        spawn_event_pipeline(
            self.entries.clone(),
            entry.clone(),
            session.clone(),
            root_name,
            generation,
        );
        LiveState {
            session,
            provider_id,
            reasoning_effort,
            generation,
            turn_running,
            snapshot,
            unsettled_users: VecDeque::new(),
            pending_approvals: Vec::new(),
            log: EventLog::default(),
        }
    }

    async fn handle_task(
        state: &mut EntryState,
        key: &SessionKey,
        task: String,
        images: Vec<String>,
    ) -> CommandOutcome {
        let EntryPhase::Live(live) = &mut state.phase else {
            return CommandOutcome::Ignored;
        };
        // Send first, record after: a failed send must not leave a phantom
        // user message or a stuck running flag.
        if let Err(err) = live.session.send(task.clone(), images.clone()).await {
            warn!(workspace_id = %key.0, session_id = %key.1, "failed to send task: {err}");
            return CommandOutcome::Ok;
        }
        live.turn_running = true;
        live.unsettled_users
            .push_back(Message::User(UserMessage::with_images(task, &images)));
        CommandOutcome::Ok
    }

    async fn handle_resume(
        &self,
        entry: &Arc<SessionEntry>,
        state: &mut EntryState,
        key: &SessionKey,
        agent_name: String,
        thread_id: String,
        decision: ResumeDecision,
    ) -> CommandOutcome {
        match &mut state.phase {
            EntryPhase::Live(live) => {
                if let Err(err) = live.session.resume(&agent_name, &thread_id, decision).await {
                    warn!(workspace_id = %key.0, session_id = %key.1, "failed to resume: {err}");
                    return CommandOutcome::Ok;
                }
                live.turn_running = true;
                live.pending_approvals
                    .retain(|approval| approval.thread_id != thread_id);
                CommandOutcome::Ok
            }
            EntryPhase::Pending(pending) => {
                pending.needed.remove(&thread_id);
                pending.decisions.insert(thread_id, decision);
                if !pending.needed.is_empty() {
                    return CommandOutcome::Ok;
                }
                let provider_id = pending.provider_id.clone();
                let reasoning_effort = pending.reasoning_effort;
                let decisions = std::mem::take(&mut pending.decisions);
                match self
                    .opener
                    .open(key, &provider_id, reasoning_effort, decisions)
                    .await
                {
                    Ok(session) => {
                        state.phase = EntryPhase::Live(self.make_live(
                            entry,
                            session,
                            provider_id,
                            reasoning_effort,
                            0,
                        ));
                        CommandOutcome::Ok
                    }
                    Err(OpenError::PendingApprovalsRequired(more)) => {
                        pending.needed = more
                            .iter()
                            .map(|approval| approval.thread_id.clone())
                            .collect();
                        pending.approvals = more.clone();
                        CommandOutcome::StillPending(more)
                    }
                    Err(err) => {
                        // Match the previous behavior: the gated open is
                        // dropped; a fresh OpenSession retries from scratch.
                        state.phase = EntryPhase::Released;
                        self.entries
                            .lock()
                            .expect("entries mutex poisoned")
                            .remove(key);
                        CommandOutcome::OpenFailed(err)
                    }
                }
            }
            _ => CommandOutcome::Ignored,
        }
    }

    async fn handle_set_model(
        &self,
        entry: &Arc<SessionEntry>,
        state: &mut EntryState,
        key: &SessionKey,
        provider_id: String,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> CommandOutcome {
        let EntryPhase::Live(live) = &mut state.phase else {
            return CommandOutcome::Ignored;
        };
        if live.provider_id == provider_id && live.reasoning_effort == reasoning_effort {
            return CommandOutcome::Ignored;
        }
        // The session is rebuilt with a new RunConfig; only safe while idle.
        if live.turn_running {
            warn!(workspace_id = %key.0, session_id = %key.1, "ignoring set_model while a turn is running");
            return CommandOutcome::Ignored;
        }
        // Open the replacement before tearing down the current session, so a
        // failed open leaves the existing one intact. The old session is idle,
        // so its checkpoint is durable and the new open reads current state.
        match self
            .opener
            .open(key, &provider_id, reasoning_effort, HashMap::new())
            .await
        {
            Ok(session) => {
                let generation = live.generation + 1;
                let mut replacement = self.make_live(
                    entry,
                    session,
                    provider_id.clone(),
                    reasoning_effort,
                    generation,
                );
                // History is unchanged by a model swap; keep the in-memory
                // snapshot rather than trusting a re-read (both match here,
                // but staying on one source keeps the invariant simple).
                replacement.snapshot = std::mem::take(&mut live.snapshot);
                let old = std::mem::replace(live, replacement);
                // The old runtime is idle; abort it outside the state mutation
                // path. Its forwarder retires on the generation bump.
                tokio::spawn(async move {
                    old.session.shutdown(Shutdown::abort()).await;
                });
                CommandOutcome::ModelChanged {
                    provider_id,
                    reasoning_effort,
                }
            }
            Err(err @ OpenError::PendingApprovalsRequired(_)) => {
                warn!(workspace_id = %key.0, session_id = %key.1, "cannot switch model while approvals are pending");
                CommandOutcome::OpenFailed(err)
            }
            Err(err) => CommandOutcome::OpenFailed(err),
        }
    }
}

impl SessionRelay for SessionHub {
    fn attach<'a>(
        &'a self,
        key: SessionKey,
        conn_id: ConnId,
        provider_id: String,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Pin<Box<dyn Future<Output = Result<AttachSession, OpenError>> + Send + 'a>> {
        Box::pin(async move {
            let (entry, mut guard) = self.lock_entry_for_attach(&key).await;
            let state = &mut *guard;

            // Latest-wins: displace whoever holds the slot. Same connection
            // re-opening is an idempotent refresh (fresh snapshot + stream).
            if let Some(previous) = state.attached.take()
                && previous.conn_id != conn_id
            {
                let _ = previous.tx.send(RelayEvent::Evicted);
                info!(workspace_id = %key.0, session_id = %key.1, "evicted previous client");
            }

            if matches!(state.phase, EntryPhase::Uninitialized) {
                match self
                    .opener
                    .open(&key, &provider_id, reasoning_effort, HashMap::new())
                    .await
                {
                    Ok(session) => {
                        state.phase = EntryPhase::Live(self.make_live(
                            &entry,
                            session,
                            provider_id,
                            reasoning_effort,
                            0,
                        ));
                        info!(workspace_id = %key.0, session_id = %key.1, "session opened");
                    }
                    Err(OpenError::PendingApprovalsRequired(approvals)) => {
                        let snapshot = self.opener.load_messages(&key).await;
                        state.phase = EntryPhase::Pending(PendingState {
                            provider_id,
                            reasoning_effort,
                            needed: approvals
                                .iter()
                                .map(|approval| approval.thread_id.clone())
                                .collect(),
                            decisions: HashMap::new(),
                            approvals,
                            snapshot,
                        });
                    }
                    Err(err) => {
                        // Don't wedge the key: drop the half-built entry.
                        state.phase = EntryPhase::Released;
                        self.entries
                            .lock()
                            .expect("entries mutex poisoned")
                            .remove(&key);
                        return Err(err);
                    }
                }
            }

            let snapshot = compose_snapshot(&state.phase)
                .expect("phase is Live or Pending after initialization");

            // Register the stream and capture the replay in the same critical
            // section the forwarder appends under: every event lands in the
            // replay xor arrives live, exactly once and in order.
            let (tx, rx) = mpsc::unbounded_channel();
            if let EntryPhase::Live(live) = &state.phase {
                for event in live.log.iter() {
                    let _ = tx.send(RelayEvent::Event(Box::new(event.clone())));
                }
            }
            state.attached = Some(Attachment { conn_id, tx });

            Ok(AttachSession {
                snapshot,
                events: UnboundedReceiverStream::new(rx).boxed(),
            })
        })
    }

    fn command<'a>(
        &'a self,
        key: SessionKey,
        conn_id: ConnId,
        command: SessionCommand,
    ) -> Pin<Box<dyn Future<Output = CommandOutcome> + Send + 'a>> {
        Box::pin(async move {
            let Some((entry, mut guard)) = self.lock_entry_for_conn(&key, conn_id).await else {
                return CommandOutcome::Ignored;
            };
            let state = &mut *guard;
            match command {
                SessionCommand::Task { task, images } => {
                    Self::handle_task(state, &key, task, images).await
                }
                SessionCommand::Resume {
                    agent_name,
                    thread_id,
                    decision,
                } => {
                    self.handle_resume(&entry, state, &key, agent_name, thread_id, decision)
                        .await
                }
                SessionCommand::Abort => {
                    if let EntryPhase::Live(live) = &state.phase {
                        live.session.abort().await;
                    }
                    CommandOutcome::Ok
                }
                SessionCommand::SetModel {
                    provider_id,
                    reasoning_effort,
                } => {
                    self.handle_set_model(&entry, state, &key, provider_id, reasoning_effort)
                        .await
                }
            }
        })
    }

    fn detach<'a>(
        &'a self,
        key: SessionKey,
        conn_id: ConnId,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let Some(entry) = self.get_entry(&key) else {
                return;
            };
            let mut guard = entry.inner.clone().lock_owned().await;
            let state = &mut *guard;
            if state
                .attached
                .as_ref()
                .is_some_and(|attachment| attachment.conn_id == conn_id)
            {
                state.attached = None;
            }
            if let Some(release) = Self::maybe_release(&self.entries, &entry, state) {
                drop(guard);
                tokio::spawn(release);
            }
        })
    }

    fn detach_all<'a>(&'a self, conn_id: ConnId) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let entries: Vec<_> = self
                .entries
                .lock()
                .expect("entries mutex poisoned")
                .values()
                .cloned()
                .collect();
            for entry in entries {
                self.detach(entry.key.clone(), conn_id).await;
            }
        })
    }

    fn delete<'a>(
        &'a self,
        key: SessionKey,
        conn_id: ConnId,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let Some(entry) = self.get_entry(&key) else {
                return true; // nothing live; persisted state is free to go
            };
            let mut guard = entry.inner.clone().lock_owned().await;
            let state = &mut *guard;
            // Latest-wins also covers destruction: only the attached client
            // (or anyone, when nobody is attached) may delete a live session.
            if state
                .attached
                .as_ref()
                .is_some_and(|attachment| attachment.conn_id != conn_id)
            {
                warn!(
                    workspace_id = %key.0,
                    session_id = %key.1,
                    "rejecting delete from a connection that is not attached"
                );
                return false;
            }
            if matches!(
                state.phase,
                EntryPhase::Releasing { .. } | EntryPhase::Released
            ) {
                // Already going away; the caller only needs the runtime gone,
                // and the release path guarantees that shortly. Wait it out.
                if let EntryPhase::Releasing { done } = &state.phase {
                    let mut done = done.clone();
                    drop(guard);
                    while !*done.borrow_and_update() {
                        if done.changed().await.is_err() {
                            break;
                        }
                    }
                }
                return true;
            }
            if let Some(attachment) = state.attached.take() {
                let _ = attachment.tx.send(RelayEvent::Evicted);
            }
            // Abort: deletion must not write a checkpoint back to disk.
            let release =
                Self::begin_release(&self.entries, &entry, state, Shutdown::abort(), false);
            drop(guard);
            // Inline: the caller deletes persisted state right after.
            release.await;
            true
        })
    }

    fn provider_of<'a>(
        &'a self,
        key: SessionKey,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(async move {
            let entry = self.get_entry(&key)?;
            let guard = entry.inner.clone().lock_owned().await;
            match &guard.phase {
                EntryPhase::Live(live) => Some(live.provider_id.clone()),
                EntryPhase::Pending(pending) => Some(pending.provider_id.clone()),
                _ => None,
            }
        })
    }

    fn shutdown_all<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let entries: Vec<_> = self
                .entries
                .lock()
                .expect("entries mutex poisoned")
                .values()
                .cloned()
                .collect();
            for entry in entries {
                let mut guard = entry.inner.clone().lock_owned().await;
                let state = &mut *guard;
                if matches!(
                    state.phase,
                    EntryPhase::Releasing { .. } | EntryPhase::Released | EntryPhase::Uninitialized
                ) {
                    continue;
                }
                let release = Self::begin_release(
                    &self.entries,
                    &entry,
                    state,
                    Shutdown::graceful_then_abort(std::time::Duration::from_secs(5)),
                    true,
                );
                drop(guard);
                release.await;
            }
        })
    }
}

impl SessionHub {
    fn get_entry(&self, key: &SessionKey) -> Option<Arc<SessionEntry>> {
        self.entries
            .lock()
            .expect("entries mutex poisoned")
            .get(key)
            .cloned()
    }
}

/// Compose the attach-time snapshot for an entry. Pure over the entry state so
/// it is unit-testable.
fn compose_snapshot(phase: &EntryPhase) -> Option<SnapshotPayload> {
    match phase {
        EntryPhase::Live(live) => {
            let mut messages = live.snapshot.clone();
            messages.extend(live.unsettled_users.iter().cloned());
            Some(SnapshotPayload {
                messages,
                pending_approvals: live.pending_approvals.clone(),
                provider_id: live.provider_id.clone(),
                reasoning_effort: live.reasoning_effort,
                turn_running: live.turn_running,
            })
        }
        EntryPhase::Pending(pending) => Some(SnapshotPayload {
            messages: pending.snapshot.clone(),
            pending_approvals: pending
                .approvals
                .iter()
                .filter(|approval| pending.needed.contains(&approval.thread_id))
                .cloned()
                .collect(),
            provider_id: pending.provider_id.clone(),
            reasoning_effort: pending.reasoning_effort,
            turn_running: false,
        }),
        _ => None,
    }
}

/// Spawn the two-stage event pipeline for a live session instance.
///
/// Stage 1 (pump) drains `session.recv()` into an unbounded channel without
/// taking any locks, keeping the broadcast receiver from lagging even while
/// the entry lock is contended (lag would drop events, which the fold can only
/// partially recover from — see the `Lagged` arm in the forwarder). Stage 2
/// (forwarder) consumes the channel and does the per-event work under the
/// entry lock.
fn spawn_event_pipeline(
    entries: Entries,
    entry: Arc<SessionEntry>,
    session: Session,
    root_name: String,
    generation: u64,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(item) = session.recv().await {
            if tx.send(item).is_err() {
                break; // forwarder retired (generation swap or release)
            }
        }
        // Dropping `tx` signals end-of-stream to the forwarder.
    });
    tokio::spawn(run_forwarder(entries, entry, rx, root_name, generation));
}

async fn run_forwarder(
    entries: Entries,
    entry: Arc<SessionEntry>,
    mut rx: mpsc::UnboundedReceiver<SessionStreamItem>,
    root_name: String,
    generation: u64,
) {
    while let Some(item) = rx.recv().await {
        let mut guard = entry.inner.clone().lock_owned().await;
        let state = &mut *guard;
        let EntryPhase::Live(live) = &mut state.phase else {
            return; // released or replaced under us
        };
        if live.generation != generation {
            return; // SetModel swapped sessions; a new forwarder owns the entry
        }
        match item {
            SessionStreamItem::Lagged(n) => {
                // The stream has a gap: the in-memory snapshot can no longer
                // be trusted to fold correctly. Drain: stop the runtime at its
                // next checkpoint boundary and drop the entry — reopening
                // reads the authoritative checkpoint from disk.
                error!(
                    workspace_id = %entry.key.0,
                    session_id = %entry.key.1,
                    "session event stream lagged by {n}; draining session to resync from disk"
                );
                let release = SessionHub::begin_release(
                    &entries,
                    &entry,
                    state,
                    Shutdown::graceful_unbounded(),
                    true,
                );
                drop(guard);
                release.await;
                return;
            }
            SessionStreamItem::Event(event) => {
                // Capture the approval before the event moves into the wire
                // conversion; recorded on settle below.
                let suspended = match &event.kind {
                    AgentEvent::Suspended(approval) => Some(approval.clone()),
                    _ => None,
                };
                let wire = WireEvent::from_session_event(event, &root_name);
                // A queued task (or restart-resume) starts a turn without a
                // command flipping the flag; the turn's first event does.
                if let WireEvent::LlmStart { agent_name, .. } = &wire
                    && agent_name == &root_name
                {
                    live.turn_running = true;
                }
                live.log.push(wire.clone());
                if let Some(attachment) = &state.attached {
                    let _ = attachment
                        .tx
                        .send(RelayEvent::Event(Box::new(wire.clone())));
                }
                if event_settles_turn(&wire, &root_name) {
                    if let Some(approval) = suspended {
                        live.pending_approvals.push(approval);
                    }
                    fold_settled_turn(
                        &mut live.snapshot,
                        &mut live.unsettled_users,
                        &mut live.log,
                        &root_name,
                    );
                    live.turn_running = false;
                    if let Some(release) = SessionHub::maybe_release(&entries, &entry, state) {
                        drop(guard);
                        release.await;
                        return;
                    }
                }
            }
        }
    }
    // The session's event stream closed: the runtime terminated on its own
    // (or a release/shutdown beat us to it). Make sure the entry is gone.
    let mut guard = entry.inner.clone().lock_owned().await;
    let state = &mut *guard;
    let retire = match &state.phase {
        EntryPhase::Live(live) => live.generation == generation,
        _ => false,
    };
    if retire {
        let release = SessionHub::begin_release(
            &entries,
            &entry,
            state,
            // The runtime is already gone; this is bookkeeping.
            Shutdown::abort(),
            true,
        );
        drop(guard);
        release.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_agent::runtime::MemoryStorage;
    use coda_agent::{
        AgentSpec, AgentTeam, ModelProfile, RunConfig, SubAgentMode, ToolApprovalMode,
        ToolCallResolution,
    };
    use coda_core::llm::{
        AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, StreamError,
        ToolCall, ToolMessage, ToolOutput,
    };
    use coda_tools::ReadTodosToolSpec;
    use futures::{Stream, StreamExt, stream};
    use std::sync::Arc;
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};

    // --- pure helpers -----------------------------------------------------

    fn assistant(content: &str) -> AssistantMessage {
        let now = jiff::Timestamp::now();
        AssistantMessage {
            content: content.into(),
            tool_calls: vec![],
            usage: None,
            reasoning_content: None,
            reasoning_ended_at: None,
            aborted: false,
            started_at: now,
            ended_at: now,
        }
    }

    fn tool_message(id: &str, text: &str) -> ToolMessage {
        ToolMessage::new(
            id.to_string(),
            "echo".to_string(),
            ToolOutput::Ok(text.to_string()),
            coda_core::llm::ToolCallOutcome::Auto,
            None,
        )
    }

    fn llm_end(agent: &str, message: AssistantMessage) -> WireEvent {
        WireEvent::LlmEnd {
            agent_name: agent.into(),
            thread_id: "t".into(),
            message,
        }
    }

    fn tool_end(agent: &str, message: ToolMessage) -> WireEvent {
        WireEvent::ToolCallEnd {
            agent_name: agent.into(),
            thread_id: "t".into(),
            message,
        }
    }

    fn chunk(agent: &str, text: &str) -> WireEvent {
        WireEvent::LlmContentChunk {
            agent_name: agent.into(),
            thread_id: "t".into(),
            content: text.into(),
        }
    }

    fn user(text: &str) -> Message {
        Message::User(UserMessage::text(text.to_string()))
    }

    // --- EventLog ----------------------------------------------------------

    #[test]
    fn event_log_overflow_drops_oldest_chunk_tier_first() {
        let mut log = EventLog::default();
        for i in 0..MAX_LOG_EVENTS {
            if i == 10 {
                log.push(tool_end("coda", tool_message("keep", "kept")));
            } else {
                log.push(chunk("coda", &format!("c{i}")));
            }
        }
        log.push(llm_end("coda", assistant("fin")));
        assert_eq!(log.entries.len(), MAX_LOG_EVENTS);
        // The oldest chunk was evicted; the message-tier events survive.
        assert!(matches!(
            log.entries.front(),
            Some(WireEvent::LlmContentChunk { content, .. }) if content == "c1"
        ));
        assert!(
            log.iter().any(
                |e| matches!(e, WireEvent::ToolCallEnd { message, .. } if message.id == "keep")
            )
        );
    }

    #[test]
    fn event_log_all_message_tier_grows_past_cap() {
        let mut log = EventLog::default();
        for i in 0..(MAX_LOG_EVENTS + 5) {
            log.push(tool_end("coda", tool_message(&format!("m{i}"), "x")));
        }
        assert_eq!(log.entries.len(), MAX_LOG_EVENTS + 5);
    }

    // --- fold_settled_turn ---------------------------------------------------

    #[test]
    fn fold_orders_stale_cleanup_before_user() {
        // History order on a stale-envelope turn: aborted ToolMessages first,
        // then the new user prompt, then the assistant reply.
        let mut snapshot = vec![];
        let mut users = VecDeque::from([user("new task")]);
        let mut log = EventLog::default();
        log.push(tool_end("coda", tool_message("stale1", "aborted")));
        log.push(tool_end("coda", tool_message("stale2", "aborted")));
        log.push(chunk("coda", "hi"));
        log.push(llm_end("coda", assistant("reply")));

        fold_settled_turn(&mut snapshot, &mut users, &mut log, "coda");

        assert_eq!(snapshot.len(), 4);
        assert!(matches!(&snapshot[0], Message::Tool(t) if t.id == "stale1"));
        assert!(matches!(&snapshot[1], Message::Tool(t) if t.id == "stale2"));
        assert!(matches!(&snapshot[2], Message::User(_)));
        assert!(matches!(&snapshot[3], Message::Assistant(a) if a.content == "reply"));
        assert!(log.entries.is_empty());
        assert!(users.is_empty());
    }

    #[test]
    fn fold_skips_subagent_and_chunk_events() {
        let mut snapshot = vec![];
        let mut users = VecDeque::from([user("task")]);
        let mut log = EventLog::default();
        log.push(chunk("coda", "x"));
        log.push(llm_end("coda", assistant("delegating")));
        log.push(llm_end("explore", assistant("sub result")));
        log.push(tool_end("explore", tool_message("sub_call", "sub")));
        log.push(tool_end(
            "coda",
            tool_message("agent_call", "reply from sub"),
        ));
        log.push(llm_end("coda", assistant("done")));

        fold_settled_turn(&mut snapshot, &mut users, &mut log, "coda");

        // user, assistant(delegating), tool(agent_call), assistant(done)
        assert_eq!(snapshot.len(), 4);
        assert!(matches!(&snapshot[0], Message::User(_)));
        assert!(matches!(&snapshot[1], Message::Assistant(a) if a.content == "delegating"));
        assert!(matches!(&snapshot[2], Message::Tool(t) if t.id == "agent_call"));
        assert!(matches!(&snapshot[3], Message::Assistant(a) if a.content == "done"));
    }

    #[test]
    fn fold_tolerates_missing_user_for_resumed_turns() {
        let mut snapshot = vec![];
        let mut users = VecDeque::new();
        let mut log = EventLog::default();
        log.push(tool_end("coda", tool_message("resolved", "ok")));
        log.push(llm_end("coda", assistant("after resume")));

        fold_settled_turn(&mut snapshot, &mut users, &mut log, "coda");

        assert_eq!(snapshot.len(), 2);
        assert!(matches!(&snapshot[0], Message::Tool(t) if t.id == "resolved"));
        assert!(matches!(&snapshot[1], Message::Assistant(_)));
    }

    // --- event_settles_turn --------------------------------------------------

    #[test]
    fn settle_ignores_aborted_llm_end() {
        let mut aborted = assistant("partial");
        aborted.aborted = true;
        assert!(!event_settles_turn(&llm_end("coda", aborted), "coda"));
        assert!(event_settles_turn(
            &llm_end("coda", assistant("done")),
            "coda"
        ));
        assert!(!event_settles_turn(
            &llm_end("explore", assistant("sub")),
            "coda"
        ));
        assert!(event_settles_turn(
            &WireEvent::Aborted {
                agent_name: "coda".into(),
                thread_id: "t".into(),
                target: crate::wire::AbortedTargetWire::Generation,
            },
            "coda"
        ));
    }

    // --- integration: hub over real sessions ---------------------------------

    #[derive(Clone)]
    struct TestProvider {
        gate: Arc<Notify>,
    }

    impl TestProvider {
        fn completed(
            message: AssistantMessage,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>>
        {
            Box::pin(stream::iter(vec![Ok(LLMStreamEvent::Completed(message))]))
        }
    }

    impl LLMProvider for TestProvider {
        fn stream(
            &self,
            request: ChatCompletionRequest,
        ) -> impl Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
            let system = request
                .messages
                .first()
                .and_then(|m| match m {
                    Message::System(s) => Some(s.0.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            match system.as_str() {
                "reply" => Self::completed(assistant("done")),
                "hold" => {
                    let gate = self.gate.clone();
                    Box::pin(
                        stream::iter(vec![Ok(LLMStreamEvent::ContentChunk("partial".into()))])
                            .chain(stream::once(async move {
                                gate.notified().await;
                                Ok(LLMStreamEvent::Completed(assistant("final")))
                            })),
                    )
                }
                // 600 chunks: far past the old broadcast capacity (128) but
                // within the new one (1024), so even a fully starved pump
                // cannot lag — the buffer holds the whole burst. (A real LLM
                // stream awaits the network per chunk so the producer yields;
                // this synchronous iter is already an adversarial case.)
                "burst" => {
                    let chunks: Vec<_> = (0..600)
                        .map(|i| Ok(LLMStreamEvent::ContentChunk(format!("c{i} "))))
                        .collect();
                    Box::pin(stream::iter(chunks).chain(Self::completed(assistant("burst done"))))
                }
                "approval" => {
                    let has_result = request
                        .messages
                        .iter()
                        .any(|m| matches!(m, Message::Tool(t) if t.name == "read_todos"));
                    if has_result {
                        Self::completed(assistant("approved-done"))
                    } else {
                        let mut msg = assistant("");
                        msg.tool_calls = vec![ToolCall {
                            id: "call_todos".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        }];
                        Self::completed(msg)
                    }
                }
                other => panic!("unexpected system prompt: {other}"),
            }
        }
    }

    struct TestOpener {
        storage: MemoryStorage,
        provider: TestProvider,
        team: AgentTeam,
        approval: ToolApprovalMode,
    }

    impl TestOpener {
        fn new(system_prompt: &str, approval: ToolApprovalMode) -> Self {
            let tools: Vec<Box<dyn coda_tools::ToolSpec>> = if system_prompt == "approval" {
                vec![Box::new(ReadTodosToolSpec)]
            } else {
                vec![]
            };
            let team = AgentTeam::new(
                AgentSpec {
                    name: "coda".into(),
                    description: String::new(),
                    system_prompt: system_prompt.into(),
                    mode: SubAgentMode::Stateful,
                    tools,
                    subagents: vec![],
                },
                vec![],
            )
            .expect("valid team");
            Self {
                storage: MemoryStorage::default(),
                provider: TestProvider {
                    gate: Arc::new(Notify::new()),
                },
                team,
                approval,
            }
        }
    }

    impl SessionOpener for TestOpener {
        fn open<'a>(
            &'a self,
            key: &'a SessionKey,
            _provider_id: &'a str,
            _reasoning_effort: Option<ReasoningEffort>,
            decisions: HashMap<String, ResumeDecision>,
        ) -> Pin<Box<dyn Future<Output = Result<Session, OpenError>> + Send + 'a>> {
            Box::pin(async move {
                Session::builder()
                    .storage(self.storage.clone())
                    .team(&self.team, ".")
                    .run_config(RunConfig {
                        default_model: ModelProfile {
                            provider: self.provider.clone(),
                            model: "fake".into(),
                            label: "fake".into(),
                            temperature: None,
                            max_completion_tokens: None,
                            reasoning_effort: None,
                        },
                        agent_models: HashMap::new(),
                        tool_approval: self.approval.clone(),
                        approval_timeout: None,
                    })
                    .session_id(key.1.clone())
                    .resume_decisions(decisions)
                    .open()
                    .await
            })
        }

        fn load_messages<'a>(
            &'a self,
            _key: &'a SessionKey,
        ) -> Pin<Box<dyn Future<Output = Vec<Message>> + Send + 'a>> {
            Box::pin(async { vec![] })
        }
    }

    fn hub_with(system_prompt: &str, approval: ToolApprovalMode) -> (SessionHub, Arc<Notify>) {
        let opener = Arc::new(TestOpener::new(system_prompt, approval));
        let gate = opener.provider.gate.clone();
        (SessionHub::new(opener), gate)
    }

    fn key() -> SessionKey {
        ("ws".to_string(), "s1".to_string())
    }

    /// Await the next `RelayEvent` matching `pred`, skipping others.
    async fn next_matching(
        events: &mut BoxStream<'static, RelayEvent>,
        pred: impl Fn(&RelayEvent) -> bool,
    ) -> RelayEvent {
        timeout(Duration::from_secs(5), async {
            loop {
                let event = events.next().await.expect("stream ended unexpectedly");
                if pred(&event) {
                    return event;
                }
            }
        })
        .await
        .expect("timed out waiting for relay event")
    }

    fn is_settling_llm_end(event: &RelayEvent) -> bool {
        matches!(
            event,
            RelayEvent::Event(e)
                if matches!(&**e, WireEvent::LlmEnd { message, .. } if message.tool_calls.is_empty())
        )
    }

    async fn wait_released(hub: &SessionHub) {
        timeout(Duration::from_secs(5), async {
            loop {
                if hub.get_entry(&key()).is_none() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("entry was not released");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn task_settles_then_reattach_shows_folded_history() {
        let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
        let attach1 = hub
            .attach(key(), 1, "prov".into(), None)
            .await
            .expect("attach");
        assert!(attach1.snapshot.messages.is_empty());
        assert!(!attach1.snapshot.turn_running);

        let mut events1 = attach1.events;
        assert!(matches!(
            hub.command(
                key(),
                1,
                SessionCommand::Task {
                    task: "hello".into(),
                    images: vec![],
                }
            )
            .await,
            CommandOutcome::Ok
        ));
        next_matching(&mut events1, is_settling_llm_end).await;

        // A second client takes over: folded history, no replay, first client
        // sees the eviction.
        let attach2 = hub
            .attach(key(), 2, "prov".into(), None)
            .await
            .expect("attach2");
        assert!(!attach2.snapshot.turn_running);
        assert_eq!(attach2.snapshot.messages.len(), 2);
        assert!(matches!(&attach2.snapshot.messages[0], Message::User(_)));
        assert!(
            matches!(&attach2.snapshot.messages[1], Message::Assistant(a) if a.content == "done")
        );
        next_matching(&mut events1, |e| matches!(e, RelayEvent::Evicted)).await;

        hub.shutdown_all().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn midturn_attach_replays_chunks_and_evicts_previous() {
        let (hub, gate) = hub_with("hold", ToolApprovalMode::Auto);
        let attach1 = hub
            .attach(key(), 1, "prov".into(), None)
            .await
            .expect("attach");
        let mut events1 = attach1.events;
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "go".into(),
                images: vec![],
            },
        )
        .await;
        // Wait until the partial chunk streamed to client 1: the turn is now
        // mid-flight.
        next_matching(&mut events1, |e| {
            matches!(e, RelayEvent::Event(ev) if matches!(&**ev, WireEvent::LlmContentChunk { .. }))
        })
        .await;

        let attach2 = hub
            .attach(key(), 2, "prov".into(), None)
            .await
            .expect("attach2");
        // Mid-turn snapshot: the user prompt is visible, the turn is running,
        // and the chunk streamed so far is replayed.
        assert!(attach2.snapshot.turn_running);
        assert!(matches!(
            attach2.snapshot.messages.last(),
            Some(Message::User(_))
        ));
        let mut events2 = attach2.events;
        next_matching(&mut events2, |e| {
            matches!(
                e,
                RelayEvent::Event(ev)
                    if matches!(&**ev, WireEvent::LlmContentChunk { content, .. } if content == "partial")
            )
        })
        .await;
        next_matching(&mut events1, |e| matches!(e, RelayEvent::Evicted)).await;

        // A stale command from the evicted client is rejected.
        assert!(matches!(
            hub.command(key(), 1, SessionCommand::Abort).await,
            CommandOutcome::Ignored
        ));

        // Release the LLM stream; client 2 sees the turn finish live.
        gate.notify_one();
        next_matching(&mut events2, is_settling_llm_end).await;

        hub.shutdown_all().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn detach_idle_releases_and_reattach_reopens_from_disk() {
        let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
        let attach1 = hub
            .attach(key(), 1, "prov".into(), None)
            .await
            .expect("attach");
        let mut events1 = attach1.events;
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "hello".into(),
                images: vec![],
            },
        )
        .await;
        next_matching(&mut events1, is_settling_llm_end).await;

        hub.detach(key(), 1).await;
        wait_released(&hub).await;

        // Reopen: history comes back from the persisted checkpoint.
        let attach2 = hub
            .attach(key(), 1, "prov".into(), None)
            .await
            .expect("re-attach");
        assert_eq!(attach2.snapshot.messages.len(), 2);
        assert!(!attach2.snapshot.turn_running);

        hub.shutdown_all().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disconnect_during_turn_keeps_session_until_settle() {
        let (hub, gate) = hub_with("hold", ToolApprovalMode::Auto);
        let attach1 = hub
            .attach(key(), 1, "prov".into(), None)
            .await
            .expect("attach");
        let mut events1 = attach1.events;
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "go".into(),
                images: vec![],
            },
        )
        .await;
        next_matching(&mut events1, |e| {
            matches!(e, RelayEvent::Event(ev) if matches!(&**ev, WireEvent::LlmContentChunk { .. }))
        })
        .await;

        // Client vanishes mid-turn: the entry must survive (turn running).
        hub.detach_all(1).await;
        assert!(hub.get_entry(&key()).is_some());

        // The turn settles with nobody attached → the entry is released, with
        // the full history checkpointed.
        gate.notify_one();
        wait_released(&hub).await;

        let attach2 = hub
            .attach(key(), 2, "prov".into(), None)
            .await
            .expect("re-attach");
        assert_eq!(attach2.snapshot.messages.len(), 2);
        assert!(
            matches!(&attach2.snapshot.messages[1], Message::Assistant(a) if a.content == "final")
        );

        hub.shutdown_all().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn burst_of_chunks_survives_replay_and_fold() {
        let (hub, _) = hub_with("burst", ToolApprovalMode::Auto);
        let attach1 = hub
            .attach(key(), 1, "prov".into(), None)
            .await
            .expect("attach");
        let mut events1 = attach1.events;
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "burst".into(),
                images: vec![],
            },
        )
        .await;
        // 600 chunks exceed the old broadcast capacity (128) while staying
        // within the new one (1024), so the burst is deterministically
        // lossless; the pump must keep the receiver drained and the turn
        // settles normally.
        next_matching(&mut events1, is_settling_llm_end).await;

        let attach2 = hub
            .attach(key(), 2, "prov".into(), None)
            .await
            .expect("attach2");
        assert_eq!(attach2.snapshot.messages.len(), 2);
        assert!(matches!(
            &attach2.snapshot.messages[1],
            Message::Assistant(a) if a.content == "burst done"
        ));

        hub.shutdown_all().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn suspended_approval_survives_release_and_promotes_on_resume() {
        let (hub, _) = hub_with("approval", ToolApprovalMode::Manual);
        let attach1 = hub
            .attach(key(), 1, "prov".into(), None)
            .await
            .expect("attach");
        let mut events1 = attach1.events;
        hub.command(
            key(),
            1,
            SessionCommand::Task {
                task: "needs approval".into(),
                images: vec![],
            },
        )
        .await;
        let suspended = next_matching(
            &mut events1,
            |e| matches!(e, RelayEvent::Event(ev) if matches!(&**ev, WireEvent::Suspended { .. })),
        )
        .await;
        let RelayEvent::Event(event) = suspended else {
            unreachable!()
        };
        let WireEvent::Suspended { approval, .. } = *event else {
            unreachable!()
        };

        // Walk away: the suspended (settled) session is released.
        hub.detach(key(), 1).await;
        wait_released(&hub).await;

        // Reopen: the checkpointed approval gates the open (Pending entry).
        let attach2 = hub
            .attach(key(), 2, "prov".into(), None)
            .await
            .expect("re-attach");
        assert_eq!(attach2.snapshot.pending_approvals.len(), 1);
        assert!(!attach2.snapshot.turn_running);
        let mut events2 = attach2.events;

        // Approving promotes the entry to live and the turn completes on the
        // stream registered at attach time.
        let outcome = hub
            .command(
                key(),
                2,
                SessionCommand::Resume {
                    agent_name: approval.agent_name.clone(),
                    thread_id: approval.thread_id.clone(),
                    decision: ResumeDecision {
                        resolutions: vec![(
                            approval.calls[0].id.clone(),
                            ToolCallResolution::Execute,
                        )],
                    },
                },
            )
            .await;
        assert!(matches!(outcome, CommandOutcome::Ok));
        next_matching(&mut events2, |e| {
            matches!(
                e,
                RelayEvent::Event(ev)
                    if matches!(&**ev, WireEvent::LlmEnd { message, .. }
                        if message.content == "approved-done")
            )
        })
        .await;

        hub.shutdown_all().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_evicts_attached_client_and_removes_entry() {
        let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
        let attach1 = hub
            .attach(key(), 1, "prov".into(), None)
            .await
            .expect("attach");
        let mut events1 = attach1.events;

        assert!(hub.delete(key(), 1).await);
        next_matching(&mut events1, |e| matches!(e, RelayEvent::Evicted)).await;
        assert!(hub.get_entry(&key()).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_from_stale_connection_is_rejected() {
        // Latest-wins covers destruction too: after being evicted, the old
        // connection must not be able to delete the session the new client is
        // driving.
        let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
        let _attach1 = hub
            .attach(key(), 1, "prov".into(), None)
            .await
            .expect("attach");
        let _attach2 = hub
            .attach(key(), 2, "prov".into(), None)
            .await
            .expect("attach2 evicts conn 1");

        assert!(!hub.delete(key(), 1).await);
        assert!(hub.get_entry(&key()).is_some());

        // The attached client itself may delete.
        assert!(hub.delete(key(), 2).await);
        assert!(hub.get_entry(&key()).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_resume_does_not_stick_turn_running() {
        // State is written only after the session accepted the command: a
        // failed resume must not flip `turn_running`, otherwise the entry
        // could never be released.
        let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
        let _attach1 = hub
            .attach(key(), 1, "prov".into(), None)
            .await
            .expect("attach");

        assert!(matches!(
            hub.command(
                key(),
                1,
                SessionCommand::Resume {
                    agent_name: "ghost".into(),
                    thread_id: "t-ghost".into(),
                    decision: ResumeDecision {
                        resolutions: vec![],
                    },
                },
            )
            .await,
            CommandOutcome::Ok
        ));
        {
            let entry = hub.get_entry(&key()).expect("entry");
            let guard = entry.inner.clone().lock_owned().await;
            let EntryPhase::Live(live) = &guard.phase else {
                panic!("expected live entry");
            };
            assert!(!live.turn_running);
            assert!(live.unsettled_users.is_empty());
        }

        // With no stuck flag, walking away releases the entry.
        hub.detach(key(), 1).await;
        wait_released(&hub).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lagged_stream_drains_session_and_closes_client() {
        // A lagged event stream means the in-memory view has a gap; the hub
        // must drain the session behind a checkpoint barrier and end the
        // client stream with `Closed` so it re-attaches from disk. Injected
        // via a parallel forwarder — the real pump makes lag (deliberately)
        // hard to reproduce.
        let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
        let attach1 = hub
            .attach(key(), 1, "prov".into(), None)
            .await
            .expect("attach");
        let mut events1 = attach1.events;

        let entry = hub.get_entry(&key()).expect("entry");
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_forwarder(
            hub.entries.clone(),
            entry,
            rx,
            "coda".into(),
            0,
        ));
        tx.send(SessionStreamItem::Lagged(42)).expect("inject lag");

        next_matching(&mut events1, |e| matches!(e, RelayEvent::Closed)).await;
        wait_released(&hub).await;

        // Reopening reads the authoritative checkpoint from disk.
        let attach2 = hub
            .attach(key(), 2, "prov".into(), None)
            .await
            .expect("re-attach");
        assert!(!attach2.snapshot.turn_running);
    }
}
