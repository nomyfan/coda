//! Process-level session relay: live agent [`Session`]s live here, decoupled
//! from the WebSocket connections that drive them.
//!
//! Connections *attach* to a session (latest-wins: attaching evicts the
//! previous client) and receive a snapshot plus a single ordered event stream
//! that replays the in-flight turn before switching to live events. A
//! disconnect merely detaches; a running turn keeps going and the session is
//! released (gracefully, its checkpoint persisted) once it is idle *and*
//! unattached.
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
use coda_tools::BackgroundProcesses;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use tokio::sync::{Mutex, OwnedMutexGuard, mpsc, watch};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{error, info, warn};

use crate::config::RelayConfig;
use crate::notices::NoticeStore;
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
    /// The command was not applied: stale connection, invalid state, or the
    /// session did not accept it (e.g. runtime channel closed). Logged;
    /// nothing to send.
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
        background: Arc<BackgroundProcesses>,
    ) -> Pin<Box<dyn Future<Output = Result<Session, OpenError>> + Send + 'a>>;

    /// Load the persisted conversation history for `key` (empty when none).
    /// Used for the snapshot of approvals-gated opens, where no live session
    /// exists yet.
    fn load_messages<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Vec<Message>> + Send + 'a>>;
}

/// Why an attach was not served.
#[derive(Debug)]
pub enum AttachError {
    /// Another connection currently holds the session and `takeover` was not
    /// requested. Nothing changed; the caller should ask the user before
    /// retrying with `takeover: true`.
    Busy,
    /// Opening the session failed.
    Open(OpenError),
}

/// The connection layer's only interface to sessions. See the module docs.
pub trait SessionRelay: Send + Sync {
    /// Open-or-attach. When another connection holds `key`: with `takeover`
    /// it is evicted (latest-wins); without, the attach fails with
    /// [`AttachError::Busy`] and nothing changes — taking a session away from
    /// another client must be an explicit user decision, not a side effect of
    /// opening it. `provider_id`/`reasoning_effort` must be pre-validated
    /// against the provider catalog; they only apply when the session is not
    /// already live.
    fn attach<'a>(
        &'a self,
        key: SessionKey,
        conn_id: ConnId,
        provider_id: String,
        reasoning_effort: Option<ReasoningEffort>,
        takeover: bool,
    ) -> Pin<Box<dyn Future<Output = Result<AttachSession, AttachError>> + Send + 'a>>;

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

/// The current turn's events, in order. Cleared when the turn settles (the
/// settled turn is folded into the entry's snapshot instead).
struct EventLog {
    entries: VecDeque<WireEvent>,
    overflowed: bool,
    /// Buffered message-tier entries; chunk-tier entries (evicted by `push`'s
    /// soft-cap eviction) don't count.
    message_tier_len: usize,
    limits: RelayConfig,
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
    fn new(limits: RelayConfig) -> Self {
        Self {
            entries: VecDeque::new(),
            overflowed: false,
            message_tier_len: 0,
            limits,
        }
    }

    fn push(&mut self, event: WireEvent) {
        // On overflow evict the oldest chunk-tier event; when the log is all
        // message-tier, let it grow here — `message_tier_overflowed` below is
        // what bounds that case, by forcing a resync instead of a silent drop.
        if self.entries.len() >= self.limits.max_log_events
            && let Some(pos) = self.entries.iter().position(is_chunk_tier)
        {
            self.entries.remove(pos);
            if !self.overflowed {
                self.overflowed = true;
                let max_log_events = self.limits.max_log_events;
                warn!(
                    "event log overflowed {max_log_events} events; \
                     dropping oldest chunk-tier events (replay will have gaps)"
                );
            }
        }
        if !is_chunk_tier(&event) {
            self.message_tier_len += 1;
        }
        self.entries.push_back(event);
    }

    /// `true` once buffered message-tier entries exceed the hard cap. Checked
    /// by the forwarder after the settle check, so a turn that just folded
    /// (clearing the log) never trips this on its own final event.
    fn message_tier_overflowed(&self) -> bool {
        self.message_tier_len > self.limits.max_message_tier_events
    }

    fn iter(&self) -> impl Iterator<Item = &WireEvent> {
        self.entries.iter()
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.overflowed = false;
        self.message_tier_len = 0;
    }
}

/// Fold the settled turn into `snapshot`, mirroring exactly what the driver
/// appended to the agent's history:
///
/// 1. Leading root `ToolCallEnd`s — stale-envelope cleanups or resume
///    resolutions, which the driver writes *before* the user message.
/// 2. The turn's user message (front of `unsettled_user_messages`; absent for resumed
///    turns).
/// 3. The remaining root `LlmEnd`/`ToolCallEnd` messages, in order.
///
/// Sub-agent events and chunk-tier events are skipped (matching what the
/// checkpoint history holds). The log is cleared afterwards.
fn fold_settled_turn(
    snapshot: &mut Vec<Message>,
    unsettled_user_messages: &mut VecDeque<Message>,
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
    if let Some(user) = unsettled_user_messages.pop_front() {
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
    /// event, so re-reading the persisted state mid-life would race — it is
    /// only read when an entry is created.
    snapshot: Vec<Message>,
    /// User messages of turns that have not settled (and thus not folded) yet.
    unsettled_user_messages: VecDeque<Message>,
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

/// A cheap, cloneable handle to a session's slot, kept separate from the
/// [`EntryGuard`] that locks its [`EntryState`] so it can be re-locked later
/// (e.g. by a spawned forwarder task) after the guard is gone.
struct SessionEntry {
    key: SessionKey,
    inner: Arc<Mutex<EntryState>>,
    /// The entry — not the `Session` — owns the background task registry, so
    /// session rebuilds within this entry (a model switch) never touch
    /// running tasks or undelivered notices. Injected into every session
    /// opened for this entry; torn down (and its notices persisted) only by
    /// `begin_release`.
    background: Arc<BackgroundProcesses>,
}

type EntryGuard = OwnedMutexGuard<EntryState>;
type Entries = Arc<std::sync::Mutex<HashMap<SessionKey, Arc<SessionEntry>>>>;

/// In-process [`SessionRelay`] implementation.
pub struct SessionHub {
    opener: Arc<dyn SessionOpener>,
    entries: Entries,
    limits: RelayConfig,
    notices: Arc<dyn NoticeStore>,
}

impl SessionHub {
    pub fn new(
        opener: Arc<dyn SessionOpener>,
        notices: Arc<dyn NoticeStore>,
        limits: RelayConfig,
    ) -> Self {
        Self {
            opener,
            entries: Arc::new(std::sync::Mutex::new(HashMap::new())),
            limits,
            notices,
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
                            background: Arc::new(BackgroundProcesses::new()),
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
        notices: Arc<dyn NoticeStore>,
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
                // Every mode used here waits unbounded for the runtime to
                // fully stop (no `Shutdown::Graceful { on_timeout: Return }`),
                // so this returning is the barrier that gates reopening the
                // key: no agent task is still running, so a subsequent open's
                // read of the persisted state can't race a checkpoint write.
                // What that checkpoint *contains* still depends on the mode:
                // `graceful_unbounded` lets an in-flight turn reach its own
                // natural stop (completed/suspended/errored) before saving,
                // so the checkpoint is current — required for the forced-
                // resync path below, which discards the in-memory view and
                // must trust the persisted state. `Abort` cancels first, so a
                // turn that was still running saves an *aborted* checkpoint
                // instead of a clean one; both call sites that use it are
                // indifferent to that (delete removes the persisted state
                // right after, and the stream-ended path only reaches here
                // once the runtime has already stopped on its own).
                session.shutdown(mode).await;
            }
            // The runtime is stopped (so no more notices can be taken for
            // delivery): tear down the entry-owned registry and persist
            // whatever is still undelivered. A failed save loses notices new
            // to this incarnation (accepted, crash-tier degradation); the
            // previous document stays intact thanks to the atomic write.
            let pending = entry.background.shutdown().await;
            if let Err(err) = notices.save(&entry.key, &pending).await {
                error!(
                    workspace_id = %entry.key.0,
                    session_id = %entry.key.1,
                    "failed to persist pending task notices: {err}"
                );
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

    /// Release the entry when nothing keeps it alive: no attached client, no
    /// running turn, and no running background task. Returns the
    /// outside-the-lock work, if any.
    fn maybe_release(
        entries: &Entries,
        entry: &Arc<SessionEntry>,
        state: &mut EntryState,
        notices: &Arc<dyn NoticeStore>,
    ) -> Option<impl Future<Output = ()> + Send + 'static> {
        if state.attached.is_some() {
            return None;
        }
        // Running background tasks pin the entry across disconnects; the
        // idle watcher re-runs this check when they hit zero.
        if entry
            .background
            .summaries()
            .borrow()
            .iter()
            .any(|summary| summary.status.is_running())
        {
            return None;
        }
        let idle = match &state.phase {
            EntryPhase::Live(live) => !live.turn_running,
            EntryPhase::Pending(_) => true,
            _ => false,
        };
        idle.then(|| {
            Self::begin_release(
                entries,
                entry,
                state,
                notices.clone(),
                Shutdown::graceful_unbounded(),
                false,
            )
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
            self.notices.clone(),
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
            unsettled_user_messages: VecDeque::new(),
            pending_approvals: Vec::new(),
            log: EventLog::new(self.limits),
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
            return CommandOutcome::Ignored;
        }
        live.turn_running = true;
        live.unsettled_user_messages
            .push_back(Message::User(UserMessage::with_images(task, &images)));
        // A task sent while approvals were pending supersedes them: the driver
        // writes the discarded calls as aborted ToolMessages (announced via
        // ToolCallEnd) and starts a fresh turn, so advertising them to a later
        // attach would offer a resume for work that no longer exists.
        live.pending_approvals.clear();
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
                    return CommandOutcome::Ignored;
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
                    .open(
                        key,
                        &provider_id,
                        reasoning_effort,
                        decisions,
                        entry.background.clone(),
                    )
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
                        // The gated open is dropped; a fresh OpenSession
                        // retries from scratch. Route through the release
                        // path rather than bare map removal: this entry is
                        // initialized, so its idle watcher is parked on the
                        // registry — only `begin_release`'s registry
                        // shutdown wakes and retires it (a bare removal
                        // would leak the watcher, its attachment, and the
                        // relay stream on every failed retry).
                        let release = Self::begin_release(
                            &self.entries,
                            entry,
                            state,
                            self.notices.clone(),
                            Shutdown::abort(),
                            false,
                        );
                        tokio::spawn(release);
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
        // The entry-owned registry carries over: running background tasks and
        // undelivered notices survive the swap (no restore here — restoring
        // is once per entry, at initialization).
        match self
            .opener
            .open(
                key,
                &provider_id,
                reasoning_effort,
                HashMap::new(),
                entry.background.clone(),
            )
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
        takeover: bool,
    ) -> Pin<Box<dyn Future<Output = Result<AttachSession, AttachError>> + Send + 'a>> {
        Box::pin(async move {
            let (entry, mut guard) = self.lock_entry_for_attach(&key).await;
            let state = &mut *guard;

            // Another client holds the slot: displacing it (latest-wins) needs
            // an explicit takeover — opening a session must not silently rip
            // it away from whoever is driving it. Same connection re-opening
            // is an idempotent refresh (fresh snapshot + stream).
            if state
                .attached
                .as_ref()
                .is_some_and(|attachment| attachment.conn_id != conn_id)
                && !takeover
            {
                return Err(AttachError::Busy);
            }
            if let Some(previous) = state.attached.take()
                && previous.conn_id != conn_id
            {
                let _ = previous.tx.send(RelayEvent::Evicted);
                info!(workspace_id = %key.0, session_id = %key.1, "evicted previous client");
            }

            if matches!(state.phase, EntryPhase::Uninitialized) {
                // Entry initialization — once per entry incarnation. Later
                // opens within this entry (model switch, approvals-gated
                // promotion) reuse the registry and must NOT restore again:
                // that would duplicate the persisted batch.
                match self.notices.load(&key).await {
                    // TODO(step 6): drop notices whose task ids already
                    // appear in the checkpoint history (origin=TaskNotice)
                    // once message origins land — the crash-window dedupe.
                    Ok(restored) => entry.background.restore_notices(restored).await,
                    Err(err) => {
                        warn!(
                            workspace_id = %key.0, session_id = %key.1,
                            "failed to load pending task notices; proceeding without: {err}"
                        );
                    }
                }
                match self
                    .opener
                    .open(
                        &key,
                        &provider_id,
                        reasoning_effort,
                        HashMap::new(),
                        entry.background.clone(),
                    )
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
                        // Restored notices die with it, but the store was not
                        // rewritten — the next attempt restores them again.
                        state.phase = EntryPhase::Released;
                        self.entries
                            .lock()
                            .expect("entries mutex poisoned")
                            .remove(&key);
                        return Err(AttachError::Open(err));
                    }
                }
                // Only after a successful init: a discarded entry (above)
                // would leave the watcher parked forever, since only
                // `begin_release` wakes it via the registry shutdown.
                spawn_idle_watcher(self.entries.clone(), entry.clone(), self.notices.clone());
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
            if let Some(release) = Self::maybe_release(&self.entries, &entry, state, &self.notices)
            {
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
            // Abort rather than graceful: a turn still in flight gets cut off
            // instead of finishing, so nothing new is added to history before
            // the caller deletes the persisted directory right after.
            let release = Self::begin_release(
                &self.entries,
                &entry,
                state,
                self.notices.clone(),
                Shutdown::abort(),
                false,
            );
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
                    self.notices.clone(),
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
            messages.extend(live.unsettled_user_messages.iter().cloned());
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
    notices: Arc<dyn NoticeStore>,
    session: Session,
    root_name: String,
    generation: u64,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    {
        let (workspace_id, session_id) = entry.key.clone();
        tokio::spawn(async move {
            info!(workspace_id = %workspace_id, session_id = %session_id, generation, "event pump started");
            while let Some(item) = session.recv().await {
                if tx.send(item).is_err() {
                    break; // forwarder retired (generation swap or release)
                }
            }
            // Dropping `tx` signals end-of-stream to the forwarder.
            info!(workspace_id = %workspace_id, session_id = %session_id, generation, "event pump stopped");
        });
    }
    tokio::spawn(run_forwarder(
        entries, entry, notices, rx, root_name, generation,
    ));
}

/// Watches the entry's background registry and re-runs the release check
/// whenever the running-task count returns to zero: `maybe_release` refuses
/// to release while tasks run, so something must revisit the decision when
/// the last one finishes (a disconnected, idle session would otherwise stay
/// resident forever). Spawned once per initialized entry; `begin_release`
/// wakes it a final time via the registry shutdown's publish, upon which it
/// observes the terminal phase and retires.
fn spawn_idle_watcher(entries: Entries, entry: Arc<SessionEntry>, notices: Arc<dyn NoticeStore>) {
    tokio::spawn(async move {
        let mut summaries = entry.background.summaries();
        loop {
            let running = summaries
                .borrow_and_update()
                .iter()
                .any(|summary| summary.status.is_running());
            if !running {
                let mut guard = entry.inner.clone().lock_owned().await;
                let state = &mut *guard;
                if matches!(
                    state.phase,
                    EntryPhase::Releasing { .. } | EntryPhase::Released
                ) {
                    return;
                }
                if let Some(release) = SessionHub::maybe_release(&entries, &entry, state, &notices)
                {
                    drop(guard);
                    release.await;
                    return;
                }
            }
            if summaries.changed().await.is_err() {
                return; // registry gone (entry dropped)
            }
        }
    });
}

/// Force the entry to drain and resync from the persisted state: used when
/// the in-memory event log can no longer be trusted (a lagged broadcast
/// receiver) or has grown past what it may safely buffer (a runaway turn).
/// `graceful_unbounded` lets any turn still in flight reach its own
/// checkpoint before the entry is removed, so the next attach reads a
/// current, authoritative persisted state instead of the discarded in-memory
/// one.
async fn force_resync(
    entries: &Entries,
    entry: &Arc<SessionEntry>,
    notices: Arc<dyn NoticeStore>,
    mut guard: EntryGuard,
    reason: String,
) {
    error!(
        workspace_id = %entry.key.0,
        session_id = %entry.key.1,
        "{reason}; draining session to resync from the persisted state"
    );
    let release = SessionHub::begin_release(
        entries,
        entry,
        &mut guard,
        notices,
        Shutdown::graceful_unbounded(),
        true,
    );
    drop(guard);
    release.await;
}

async fn run_forwarder(
    entries: Entries,
    entry: Arc<SessionEntry>,
    notices: Arc<dyn NoticeStore>,
    mut rx: mpsc::UnboundedReceiver<SessionStreamItem>,
    root_name: String,
    generation: u64,
) {
    info!(workspace_id = %entry.key.0, session_id = %entry.key.1, generation, "event forwarder started");
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
                // be trusted to fold correctly.
                force_resync(
                    &entries,
                    &entry,
                    notices,
                    guard,
                    format!("session event stream lagged by {n}"),
                )
                .await;
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
                        &mut live.unsettled_user_messages,
                        &mut live.log,
                        &root_name,
                    );
                    live.turn_running = false;
                    if let Some(release) =
                        SessionHub::maybe_release(&entries, &entry, state, &notices)
                    {
                        drop(guard);
                        release.await;
                        return;
                    }
                } else if live.log.message_tier_overflowed() {
                    // The turn hasn't settled and won't stop buffering
                    // message-tier history (which can't be evicted without
                    // corrupting the fold); force the same forced-resync path
                    // as a lagged stream rather than grow unbounded.
                    let max = live.log.limits.max_message_tier_events;
                    let reason = format!("event log exceeded {max} buffered message-tier events");
                    force_resync(&entries, &entry, notices, guard, reason).await;
                    return;
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
            notices,
            // The runtime is already gone; this is bookkeeping.
            Shutdown::abort(),
            true,
        );
        drop(guard);
        release.await;
    }
    info!(workspace_id = %entry.key.0, session_id = %entry.key.1, generation, "event forwarder stopped");
}

#[cfg(test)]
#[path = "hub_tests.rs"]
mod tests;
