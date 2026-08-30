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
    runtime::SendCommandError,
};
use coda_background::{ArchiveDir, BackgroundProcesses, TaskNotice, TaskSummary};
use coda_core::llm::{Message, MessageId, TaskNoticeMessage, TurnId, UserMessage};
use futures::StreamExt as _;
use futures::stream::BoxStream;
use tokio::sync::{Mutex, OwnedMutexGuard, broadcast, mpsc, watch};
use tokio_stream::wrappers::{BroadcastStream, UnboundedReceiverStream};
use tracing::{error, info, warn};

use crate::config::{PermissionMode, PermissionModeCell, RelayConfig};
use crate::storage::{ForkCut, ForkError, ForkSource, ForkedSession, RewindError, UnseenOutcome};
use crate::wire::WireEvent;

/// Buffer size per lagging status-broadcast subscriber. A dropped event only
/// delays a live update; the catalog remains the source of truth.
const STATUS_BROADCAST_CAPACITY: usize = 256;

pub type SessionKey = (String, String); // (workspace_id, session_id)
pub type ConnId = u64;

/// How long a runtime the hub has already judged broken is given to stop
/// politely. Short on purpose: the entry has to come back either way, and
/// whatever the runtime would still write is the state that just failed to
/// land.
const BROKEN_RUNTIME_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

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
    /// Discard `target` and everything after it, then start a fresh turn from
    /// the edited text. One command rather than two so no other command can
    /// land between the truncation and the turn that replaces it.
    Rewind {
        target: MessageId,
        task: String,
        images: Vec<String>,
    },
    Abort,
    SetModel {
        provider_id: String,
        reasoning_effort: Option<String>,
    },
    /// Change how much this session may do unattended. Unlike `SetModel` this
    /// needs no rebuild — the runtime reads the mode through a shared cell —
    /// so it is accepted mid-turn and mid-suspension, and applies to the next
    /// approval check rather than to calls already parked.
    SetPermissionMode {
        mode: PermissionMode,
    },
    /// Summarize the root thread and append the summary, so later turns are
    /// built from it instead of the whole conversation. Unlike every other
    /// command this lets go of the entry lock partway through — the summary is
    /// a full LLM round-trip.
    Compact {
        instructions: String,
    },
    /// SIGKILL a background task's process group. Needs no live runtime — the
    /// registry belongs to the entry — so it works while a turn is in flight,
    /// which is exactly when a user watching a runaway task wants it.
    KillTask {
        task_id: String,
    },
}

/// An element of the per-attachment event stream.
#[derive(Debug)]
pub enum RelayEvent {
    Event(Box<WireEvent>),
    /// The session's background task list changed. Not part of the event
    /// stream: tasks outlive turns, so their comings and goings are not a
    /// turn's history.
    BackgroundTasks(Arc<[TaskSummary]>),
    /// The runtime opened a turn with a background-task notice. The event
    /// stream carries no user-role messages — a human's own message is the
    /// client's optimistic copy — so a message nobody typed has to be handed
    /// over explicitly, or an attached client would not see it until it
    /// re-attached. Sent under the entry lock, ahead of that turn's events.
    TaskNotice(Box<TaskNoticeMessage>),
    /// The session's state changed outside the event stream and the attached
    /// client needs the whole picture again. Unlike the two below, the stream
    /// continues after it.
    Snapshot(Box<SnapshotPayload>),
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
    pub reasoning_effort: Option<String>,
    /// The mode the session is actually running under, which is not
    /// necessarily the one the attaching client asked for: a client that
    /// reconnects to a session still running from an earlier attachment adopts
    /// this value rather than imposing its own.
    pub permission_mode: PermissionMode,
    /// A turn is in flight; its events so far are replayed at the head of the
    /// attach stream.
    pub turn_running: bool,
    /// A compaction is in flight. It is not a turn and produces no events, so
    /// without this a client attaching mid-compaction would read the session as
    /// idle and let the user send — only to be refused.
    pub compacting: bool,
    /// Every background task the session still knows about, so an attaching
    /// client starts with the list rather than waiting for the next change.
    pub background_tasks: Arc<[TaskSummary]>,
}

pub struct AttachSession {
    pub snapshot: SnapshotPayload,
    /// Replay of the current turn followed seamlessly by live events. Ends
    /// after [`RelayEvent::Evicted`] / [`RelayEvent::Closed`], or silently on
    /// detach/release.
    pub events: BoxStream<'static, RelayEvent>,
}

/// A session's unseen outcome just changed; see [`SessionRelay::subscribe_status`].
#[derive(Debug, Clone)]
pub struct SessionStatusEvent {
    pub workspace_id: String,
    pub session_id: String,
    pub outcome: UnseenOutcome,
}

/// Result of [`SessionRelay::command`], driving the connection layer's
/// client-facing responses.
pub enum CommandOutcome {
    /// The command was accepted (or was a benign no-op).
    Ok,
    /// A `Task` was accepted, carrying the id minted for the user message it
    /// became. The request dispatcher answers the client with it so the client
    /// and the server name that message the same way.
    TaskAccepted { message_id: MessageId },
    /// The command was not applied: stale connection, invalid state, or the
    /// session did not accept it (e.g. runtime channel closed). Logged;
    /// nothing to send. For a `SetModel`, the request dispatcher reads this as
    /// `SESSION_NOT_LIVE` (the stale/not-attached guard *and* the non-`Live`
    /// phase both land here — see Decision 8).
    Ignored,
    /// A `Resume` against an approvals-gated open that still needs more
    /// decisions; the client should be shown these approvals.
    StillPending(Vec<PendingApproval>),
    /// A `Rewind` succeeded: the history that survived it, and the id minted
    /// for the edited message that now follows it.
    Rewound {
        message_id: MessageId,
        messages: Vec<Message>,
    },
    /// A command refused because the session is not at rest — a turn is in
    /// flight, or something is waiting on a human. Nothing was changed.
    NotIdle,
    /// A `Rewind` naming a message that is not a user message of this session's
    /// root thread. Nothing was discarded.
    RewindTargetNotFound,
    /// A `Rewind` whose truncation committed but whose replacement turn never
    /// started. The runtime is gone and the client has been told to re-attach,
    /// which is what puts it back in step with the truncated history.
    RewindNotStarted,
    /// A `SetModel` was applied.
    ModelChanged {
        provider_id: String,
        reasoning_effort: Option<String>,
    },
    /// A `SetModel` selecting the model already in effect: a benign no-op the
    /// request dispatcher reports as idempotent success (echoing the selection).
    Unchanged,
    /// Provider/model is immutable after the session is opened. Only the
    /// reasoning effort of that exact model can be changed.
    ModelLocked,
    /// A `SetModel` rejected because a turn is in flight (the session can only be
    /// rebuilt while idle). Reported as `MODEL_SWITCH_WHILE_RUNNING`.
    TurnRunning,
    /// A `Compact` wrote its two messages. `applied` is false when the summary
    /// could not be generated: the record is in the transcript, but the
    /// boundary has not moved and the model still sees everything.
    ///
    /// The messages themselves are not here on purpose — the client gets them
    /// from the snapshot pushed alongside, so only one path writes the
    /// transcript and it cannot render them twice.
    Compacted { applied: bool },
    /// A `Compact` that wrote nothing: the conversation moved on while the
    /// summary was being generated, or the write failed.
    CompactionAbandoned { stale: bool, reason: String },
    /// A `Compact` refused because the root thread has nothing to summarize.
    CompactionEmpty,
    /// The replacement runtime was valid, but persisting its effort failed;
    /// the current live runtime remains unchanged.
    PersistenceFailed(String),
    /// Opening the session failed (approvals-gated promotion or `SetModel`).
    OpenFailed(OpenError),
}

/// Result of [`SessionRelay::fork`].
pub enum ForkOutcome {
    Forked(ForkedSession),
    /// The source is not at rest — a turn is in flight, something is waiting on
    /// a human, or a task is queued behind the current one.
    NotIdle,
    Failed(ForkError),
}

/// Result of [`SessionRelay::delete`].
#[derive(Clone, Debug)]
pub enum DeleteOutcome {
    Deleted,
    /// Another connection currently holds the session.
    NotOwner,
    /// A compaction is running. Deleting would waste the summary and race the
    /// commit; wait it out.
    NotIdle,
    /// The runtime is gone but the persisted state is still (at least partly)
    /// there. The session survives and can be reopened, so the client must be
    /// told this failed rather than shown a catalog it has vanished from.
    Failed(String),
}

/// What a compaction wrote to the root thread. Both messages are always
/// written; `applied` says whether the second one is the summary — and so the
/// new boundary — or a record of why there is none.
pub struct Compacted {
    pub command: Message,
    pub outcome: Message,
    pub applied: bool,
}

/// Why a compaction wrote nothing at all. The client's transcript is untouched,
/// so a retry needs no cleanup.
#[derive(Debug)]
pub enum CompactError {
    /// The root thread grew while the summary was being generated, so the
    /// summary no longer describes the whole thread.
    Stale,
    /// There is no root-thread history to summarize.
    Empty,
    /// The provider-facing history has an invalid tool-call/result sequence.
    InvalidHistory(String),
    Storage(String),
}

/// Builds sessions for the relay. Injected at construction: configuration is
/// available on every instance, so commands never need to carry build logic.
pub trait SessionOpener: Send + Sync + 'static {
    /// Open (or resume) the session for `key`, seeded with `decisions` for any
    /// pending approvals carried over from a prior suspension.
    ///
    /// `permission_mode` is handed over as a shared cell rather than a value:
    /// the runtime's approval closure keeps reading it, so the caller can change
    /// the session's posture later without opening a new one.
    fn open<'a>(
        &'a self,
        key: &'a SessionKey,
        provider_id: &'a str,
        reasoning_effort: Option<String>,
        permission_mode: PermissionModeCell,
        decisions: HashMap<String, ResumeDecision>,
        background: Arc<BackgroundProcesses>,
    ) -> Pin<Box<dyn Future<Output = Result<Session, OpenError>> + Send + 'a>>;

    /// Open the on-disk archive holding `key`'s background task output. Its
    /// root outlives any one entry, so output a task produced before a release
    /// is still readable after the reopen; `Err` disables background work for
    /// the session without otherwise affecting it.
    fn background_archive(&self, key: &SessionKey) -> Result<ArchiveDir, String>;

    /// Remove everything `key` durably owns — its stored session and its
    /// background task spool.
    ///
    /// Called from inside the hub's delete tombstone, with the runtime stopped
    /// and the task registry closed, so nothing is still writing to either.
    /// `Err` leaves the session reachable: the caller frees the key regardless,
    /// and a client that reopens it must find it there.
    fn delete_persisted<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Load the persisted conversation history for `key` (empty when none).
    /// Used for the snapshot of approvals-gated opens, where no live session
    /// exists yet.
    fn load_messages<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Vec<Message>> + Send + 'a>>;

    /// Discard `target` and everything the session produced from it onward,
    /// returning the root thread's remaining conversation.
    ///
    /// The caller must have stopped the session's runtime first; this cannot
    /// check that. Fails without changing anything when `target` is not a user
    /// message of the session's root thread, or when the session still has work
    /// parked somewhere.
    fn rewind<'a>(
        &'a self,
        key: &'a SessionKey,
        target: MessageId,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Message>, RewindError>> + Send + 'a>>;

    /// Copy `key` under a freshly minted session id. A cut keeps the turns
    /// before the user message it names; an uncut copy keeps everything stored.
    /// The source is only read.
    fn fork<'a>(
        &'a self,
        key: &'a SessionKey,
        cut: ForkCut,
        source: ForkSource,
    ) -> Pin<Box<dyn Future<Output = Result<ForkedSession, ForkError>> + Send + 'a>>;

    /// Summarize the root thread since its last compaction and append the
    /// result to it, returning the two messages written.
    ///
    /// The caller must have gated the session idle and marked it compacting,
    /// and must **not** hold the entry lock: this makes a full LLM round-trip.
    /// The model binding is passed in rather than read from storage, so a
    /// compaction always uses the selection the session is actually running.
    ///
    /// A summary that cannot be generated is still recorded — `applied` is then
    /// false and the boundary has not moved. Only [`CompactError`] means
    /// nothing was written.
    fn compact<'a>(
        &'a self,
        key: &'a SessionKey,
        provider_id: &'a str,
        reasoning_effort: Option<&'a str>,
        instructions: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Compacted, CompactError>> + Send + 'a>>;

    /// Persist an effort update after a replacement runtime has been built but
    /// before it becomes live.
    fn update_reasoning_effort<'a>(
        &'a self,
        key: &'a SessionKey,
        provider_id: &'a str,
        reasoning_effort: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Record that `key`'s turn just settled with nobody attached. Best-effort.
    fn mark_unseen_outcome<'a>(
        &'a self,
        key: &'a SessionKey,
        outcome: UnseenOutcome,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Clear any unseen outcome recorded for `key`. Called on every attach.
    fn clear_unseen_outcome<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
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
    /// against the provider catalog; they — and `permission_mode` — only apply
    /// when the session is not already live. A session that is still running
    /// keeps the posture it was started with, and reports it back in the
    /// snapshot.
    #[allow(clippy::too_many_arguments)]
    fn attach<'a>(
        &'a self,
        key: SessionKey,
        conn_id: ConnId,
        provider_id: String,
        reasoning_effort: Option<String>,
        permission_mode: PermissionMode,
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
    /// checkpoint write-back), *including* everything it persisted — the
    /// stored session and its background task spool go with it, inside the
    /// same barrier, so no attach can reopen the key while they are being
    /// removed. [`DeleteOutcome::Deleted`] means it is all durably gone;
    /// [`DeleteOutcome::Failed`] means the runtime stopped but the session is
    /// still there and can be reopened.
    /// [`DeleteOutcome::NotOwner`] is a stale client trying to erase work
    /// another connection is driving (latest-wins). [`DeleteOutcome::NotIdle`]
    /// is a running compaction: the summary is in flight with no lock held, and
    /// deleting now would only waste it. Unattached idle sessions (e.g. deleting
    /// history from the catalog) are fair game for any connection.
    fn delete<'a>(
        &'a self,
        key: SessionKey,
        conn_id: ConnId,
    ) -> Pin<Box<dyn Future<Output = DeleteOutcome> + Send + 'a>>;

    /// Copy `source` under a new session id, keeping the turns before the user
    /// message named by `cut` (`None` copies everything stored).
    ///
    /// The source is left untouched, so unlike `delete` this needs no
    /// latest-wins check: any connection may fork any session. It is refused
    /// only when the source is not at rest.
    fn fork<'a>(
        &'a self,
        source: SessionKey,
        cut: Option<MessageId>,
    ) -> Pin<Box<dyn Future<Output = ForkOutcome> + Send + 'a>>;

    /// The provider a live (or pending) session was opened with.
    fn provider_of<'a>(
        &'a self,
        key: SessionKey,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

    /// Gracefully stop every session (process shutdown).
    fn shutdown_all<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// A live feed of unseen-outcome changes across every session on this
    /// process. Broadcast and best-effort: a lagging receiver misses events,
    /// but the catalog stays correct regardless.
    fn subscribe_status(&self) -> BoxStream<'static, SessionStatusEvent>;

    /// Session ids in `workspace_id` with a turn currently in flight,
    /// regardless of attachment. A point-in-time read, not a subscription.
    fn running_sessions<'a>(
        &'a self,
        workspace_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = HashSet<String>> + Send + 'a>>;
}

/// True when `event` ends the current turn: the root agent's final `LlmEnd`
/// (no tool calls, not aborted — an aborted partial message is always followed
/// by the `Aborted` marker, which is the single settle signal for that path),
/// any suspension, or the root agent aborting/erroring.
///
/// `PersistFailed` is deliberately not among them: a turn whose content never
/// reached the database has not finished, whatever the screen already shows.
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

/// How a settling `event` (one `event_settles_turn` already accepted) should
/// be recorded if it turns out to have settled unattended. Suspensions are
/// not classified here — the caller skips them entirely, since
/// `has_pending_approval` already covers that case.
fn unseen_outcome_for(event: &WireEvent) -> UnseenOutcome {
    match event {
        WireEvent::Aborted { .. } | WireEvent::Error { .. } => UnseenOutcome::Failed,
        _ => UnseenOutcome::Completed,
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
            | WireEvent::CompactionStart { .. }
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
/// 2. The turn's user message (`unsettled_user_message`; absent for resumed turns).
/// 3. The remaining root `LlmEnd`/`ToolCallEnd`/`CompactionEnd` messages, in
///    order.
///
/// Sub-agent events and chunk-tier events are skipped (matching what the
/// checkpoint history holds). The log is cleared afterwards. Returns whether a
/// different turn's user message remains unsettled.
fn fold_settled_turn(
    snapshot: &mut Vec<Message>,
    unsettled_user_message: &mut Option<(TurnId, Message)>,
    log: &mut EventLog,
    root_name: &str,
    settled: TurnId,
) -> bool {
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
    if unsettled_user_message
        .as_ref()
        .is_some_and(|(turn, _)| *turn == settled)
        && let Some((_, message)) = unsettled_user_message.take()
    {
        snapshot.push(message);
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
            WireEvent::CompactionEnd {
                agent_name,
                message,
                ..
            } if agent_name == root_name => snapshot.push(Message::Compaction(message.clone())),
            _ => {}
        }
    }
    log.clear();
    unsettled_user_message.is_some()
}

struct Attachment {
    conn_id: ConnId,
    tx: mpsc::UnboundedSender<RelayEvent>,
}

struct LiveState {
    session: Session,
    provider_id: String,
    reasoning_effort: Option<String>,
    /// Bumped when `SetModel` swaps the underlying session so the previous
    /// forwarder retires itself.
    generation: u64,
    turn_running: bool,
    /// The settled conversation history, kept in memory and used for attach
    /// snapshots. Built up here rather than re-read from storage, which is why
    /// it is only loaded when an entry is created.
    ///
    /// It used to be re-reading that was unsafe — the driver's final checkpoint
    /// landed *after* the settle event, so the database was briefly behind what
    /// had already been announced. That ordering is reversed now; what remains
    /// is simply that this is the relay's own composed view.
    snapshot: Vec<Message>,
    /// The active turn's user message until its first settle folds it into the
    /// snapshot. The id makes repeated settlement of that turn idempotent.
    unsettled_user_message: Option<(TurnId, Message)>,
    pending_approvals: Vec<PendingApproval>,
    log: EventLog,
}

struct PendingState {
    provider_id: String,
    reasoning_effort: Option<String>,
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
    Live(Box<LiveState>),
    /// Approvals-gated open: no runtime yet, resume decisions being collected.
    Pending(PendingState),
    /// Shutdown in progress outside the lock; `done` flips true after the
    /// entry is removed from the map. `watch` carries the value, so a waiter
    /// that subscribes after completion still observes it (no missed wakeup).
    Releasing {
        done: watch::Receiver<bool>,
    },
    /// Delete in progress outside the lock. Like `Releasing`, except the
    /// barrier reaches past the runtime: the stored session and its task spool
    /// go too, and `done` only carries an outcome once all of that has happened
    /// and the map slot is gone. Anyone arriving meanwhile waits here rather
    /// than opening a session whose rows are on their way out.
    Deleting {
        done: watch::Receiver<Option<DeleteOutcome>>,
    },
    /// Tombstone: this Arc is (about to be) gone from the map; retry there.
    Released,
}

/// Wait for a delete tombstone to publish its outcome.
///
/// A sender dropped without publishing means the delete task died mid-way; the
/// persisted state is then in an unknown state, which is a failure, not a
/// silent success.
async fn await_delete(done: &watch::Receiver<Option<DeleteOutcome>>) -> DeleteOutcome {
    let mut done = done.clone();
    loop {
        // watch carries the value, so a delete that finished before we
        // subscribed is still observed (no missed wakeup).
        let current = done.borrow_and_update().clone();
        if let Some(outcome) = current {
            return outcome;
        }
        if done.changed().await.is_err() {
            return DeleteOutcome::Failed("the delete did not report an outcome".into());
        }
    }
}

/// What a source session's live state says about forking it.
enum ForkGate {
    /// At rest.
    Ready,
    /// Nothing live — the stored state is all there is.
    Cold,
    Busy,
}

struct EntryState {
    phase: EntryPhase,
    /// The single attached client — the latest-wins slot.
    attached: Option<Attachment>,
    /// The session's live approval posture, shared with the approval closure
    /// inside its runtime. It sits on the entry rather than inside a phase
    /// because it has to survive both transitions a session can make: the
    /// `Pending` → `Live` promotion and the `SetModel` rebuild.
    ///
    /// Its value is set by whichever attach initializes the phase and is
    /// authoritative from then on — a later attach carries the client's
    /// remembered mode, which is exactly the value a live session must
    /// *not* be reset to.
    permission_mode: PermissionModeCell,
    /// A compaction is running with the lock released. It sits on the entry for
    /// the same reason `permission_mode` does, and more sharply: `rewind` and
    /// `SetModel` both replace the whole `LiveState` via `make_live`, so a flag
    /// kept in the phase would vanish under a compaction and leave the entry
    /// looking idle — free to be released, or to accept a second compaction.
    compacting: bool,
    /// The session's background task registry, created by the attach that
    /// initializes the entry. It sits here rather than in a phase for the same
    /// reason `permission_mode` does, and more: a task belongs to the session,
    /// so it has to outlive the `SetModel` rebuild that replaces the whole
    /// runtime underneath it.
    background: Option<Arc<BackgroundProcesses>>,
    /// Task notices waiting for a turn to deliver them in. Drained from the
    /// registry as soon as they appear; each one opens its own turn, so they
    /// queue here whenever the session is busy. A non-empty queue keeps the
    /// entry alive — releasing with notices still in hand would drop them.
    pending_notices: VecDeque<TaskNotice>,
    /// Stops the notice watcher when the entry goes away. Without it the task
    /// would park on a watch nothing will ever publish to again.
    notice_watcher: Option<tokio::task::AbortHandle>,
}

/// A cheap, cloneable handle to a session's slot, kept separate from the
/// [`EntryGuard`] that locks its [`EntryState`] so it can be re-locked later
/// (e.g. by a spawned forwarder task) after the guard is gone.
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
    limits: RelayConfig,
    status_tx: broadcast::Sender<SessionStatusEvent>,
}

impl SessionHub {
    pub fn new(opener: Arc<dyn SessionOpener>, limits: RelayConfig) -> Self {
        let (status_tx, _) = broadcast::channel(STATUS_BROADCAST_CAPACITY);
        Self {
            opener,
            entries: Arc::new(std::sync::Mutex::new(HashMap::new())),
            limits,
            status_tx,
        }
    }

    /// Get or insert the entry for `key`, waiting out any in-flight release or
    /// delete. Returns with the entry lock held and the phase not
    /// `Releasing`/`Deleting`/`Released`.
    ///
    /// Shared by attach, fork and delete: taking the same gate is what makes
    /// them serialize on a key none of them found live, since the slot this
    /// inserts is the only thing they have to lock.
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
                                permission_mode: PermissionModeCell::default(),
                                compacting: false,
                                background: None,
                                pending_notices: VecDeque::new(),
                                notice_watcher: None,
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
                EntryPhase::Deleting { done } => {
                    let done = done.clone();
                    drop(guard);
                    // The delete owns the key until it publishes: waiting here
                    // is what stops an attach from opening a session whose rows
                    // and spool are still being removed. Its outcome is not our
                    // business — loop and take whatever the map holds now.
                    let _ = await_delete(&done).await;
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
        if let Some(watcher) = state.notice_watcher.take() {
            watcher.abort();
        }
        let phase = std::mem::replace(&mut state.phase, EntryPhase::Releasing { done: done_rx });
        let session = match phase {
            EntryPhase::Live(live) => Some(live.session),
            _ => None,
        };
        let background = state.background.take();
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
            if let Some(background) = background {
                // The hub owns injected registries. Runtime shutdown comes
                // first so no agent can start another task while the registry
                // closes, kills its process groups, and joins every monitor.
                let _ = background.shutdown().await;
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

    /// Transition the entry to `Deleting` and spawn the whole delete to run
    /// *outside* the entry lock: stop the runtime, close the task registry,
    /// remove everything the session persisted, and only then drop the map slot
    /// and publish the outcome on the returned watch.
    ///
    /// The tombstone is the point. Stopping the runtime and deleting what it
    /// wrote are one transaction, and it used to be split across two callers:
    /// the hub freed the key as soon as the runtime was down, leaving an attach
    /// free to open the session again while its rows and its task spool were
    /// still being removed underneath it. Holding the entry across the whole
    /// thing closes that window — including for a key nothing was live on,
    /// which is why the caller borrows a slot to put this in.
    ///
    /// The hub spawns it rather than handing the future to the caller: only
    /// the delete finishing lifts the tombstone, so a caller dropped mid-way
    /// (a request task aborted with its connection) would leave the key held
    /// forever, with every later attach spinning on it.
    fn begin_delete(
        &self,
        entry: &Arc<SessionEntry>,
        state: &mut EntryState,
    ) -> watch::Receiver<Option<DeleteOutcome>> {
        let (done_tx, done_rx) = watch::channel(None);
        if let Some(watcher) = state.notice_watcher.take() {
            watcher.abort();
        }
        let phase = std::mem::replace(
            &mut state.phase,
            EntryPhase::Deleting {
                done: done_rx.clone(),
            },
        );
        let session = match phase {
            EntryPhase::Live(live) => Some(live.session),
            _ => None,
        };
        let background = state.background.take();
        let entries = self.entries.clone();
        let opener = self.opener.clone();
        let entry = entry.clone();
        tokio::spawn(async move {
            if let Some(session) = session {
                // Abort rather than graceful: a turn still in flight gets cut
                // off instead of finishing, so no checkpoint is written back
                // after the rows it belongs to are gone.
                session.shutdown(Shutdown::abort()).await;
            }
            if let Some(background) = background {
                // Runtime first, then the registry the hub owns: no agent can
                // start another task while it kills its process groups and
                // joins every monitor. Both are done before anything on disk
                // goes, so nothing is still writing into what we remove.
                let _ = background.shutdown().await;
            }
            let outcome = match opener.delete_persisted(&entry.key).await {
                Ok(()) => DeleteOutcome::Deleted,
                Err(error) => {
                    warn!(
                        workspace_id = %entry.key.0,
                        session_id = %entry.key.1,
                        "failed to delete persisted session state: {error}"
                    );
                    // The session is still there, so the slot has to be freed
                    // either way — a client that reopens it must be served, not
                    // parked on a tombstone nothing will ever lift.
                    DeleteOutcome::Failed(error)
                }
            };
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
            if matches!(outcome, DeleteOutcome::Deleted) {
                info!(workspace_id = %entry.key.0, session_id = %entry.key.1, "session deleted");
            }
            let _ = done_tx.send(Some(outcome));
        });
        done_rx
    }

    /// Give back the entry a fork or a refused delete took the gate on.
    ///
    /// When the caller created it, the slot has to go — but a tombstone comes
    /// first: an attach may already hold this `Arc` and be blocked on the mutex,
    /// and seeing `Released` sends it back to the map for a fresh slot instead of
    /// opening a runtime on an entry nothing can look up.
    fn leave_entry_gate(
        entries: &Entries,
        entry: &Arc<SessionEntry>,
        state: &mut EntryState,
        borrowed: bool,
    ) {
        if !borrowed {
            return;
        }
        state.phase = EntryPhase::Released;
        let mut map = entries.lock().expect("entries mutex poisoned");
        if map
            .get(&entry.key)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            map.remove(&entry.key);
        }
    }

    /// Release the entry when nothing keeps it alive: no attached client and
    /// no running turn. Returns the outside-the-lock work, if any.
    async fn maybe_release(
        entries: &Entries,
        entry: &Arc<SessionEntry>,
        state: &mut EntryState,
    ) -> Option<impl Future<Output = ()> + Send + 'static> {
        if state.attached.is_some() || state.compacting {
            return None;
        }
        let idle = match &state.phase {
            EntryPhase::Live(live) => !live.turn_running,
            EntryPhase::Pending(_) => true,
            _ => false,
        };
        if !idle {
            return None;
        }
        // Background work keeps the session alive on its own: a running task
        // has a completion to report, and a notice already in hand has one to
        // deliver. The registry check also waits out detached archive/quota
        // work, which can stage an expiration after its original waiter was
        // cancelled.
        if !state.pending_notices.is_empty() {
            return None;
        }
        if let Some(background) = state.background.clone() {
            let fresh = background.take_notices_if_quiescent().await?;
            state.pending_notices.extend(fresh);
            if !state.pending_notices.is_empty() {
                return None;
            }
        }
        Some(Self::begin_release(
            entries,
            entry,
            state,
            Shutdown::graceful_unbounded(),
            false,
        ))
    }

    /// The entry's task registry, created on first use by whichever attach
    /// initializes the entry — which is also when its watcher starts. Opening
    /// the archive can fail; a disabled registry keeps the conversation
    /// working with background execution turned off.
    async fn ensure_background(
        &self,
        entry: &Arc<SessionEntry>,
        state: &mut EntryState,
    ) -> Arc<BackgroundProcesses> {
        if let Some(background) = &state.background {
            return background.clone();
        }
        let background = Arc::new(match self.opener.background_archive(&entry.key) {
            Ok(archive) => BackgroundProcesses::session_backed(archive).await,
            Err(error) => {
                warn!(
                    workspace_id = %entry.key.0,
                    session_id = %entry.key.1,
                    "background task archive unavailable: {error}"
                );
                BackgroundProcesses::disabled_from(error)
            }
        });
        state.background = Some(background.clone());
        state.notice_watcher = Some(spawn_notice_watcher(
            self.entries.clone(),
            entry.clone(),
            background.clone(),
        ));
        background
    }

    /// Hand every pending notice to the session as one turn. Returns whether a
    /// turn was started, which is also what tells the caller the entry is no
    /// longer idle.
    async fn deliver_pending_notices(state: &mut EntryState, key: &SessionKey) -> bool {
        if state.compacting {
            return false;
        }
        // Take whatever the registry is holding first. The watcher drains on
        // its own schedule — one wake per task settling — so without this a
        // turn ending mid-flurry would carry only the notices that happened to
        // have been drained by then, and the rest would each get a turn.
        if let Some(background) = state.background.clone() {
            let fresh = background.take_notices().await;
            state.pending_notices.extend(fresh);
        }
        if state.pending_notices.is_empty() {
            return false;
        }
        let EntryPhase::Live(live) = &mut state.phase else {
            return false;
        };
        // The runtime runs one root turn at a time, and a suspension does not
        // end one — a session parked on an approval holds its notices until a
        // human answers.
        if live.turn_running
            || !live.pending_approvals.is_empty()
            || live.unsettled_user_message.is_some()
        {
            return false;
        }
        // Everything waiting goes in one message. Tasks that finished while a
        // turn ran are one interruption, not one each; what cannot be merged is
        // a notice that arrives after this has gone out, and that one simply
        // gets the next turn.
        let notices: Vec<TaskNotice> = state.pending_notices.drain(..).collect();
        let text = notices
            .iter()
            .map(TaskNotice::render)
            .collect::<Vec<_>>()
            .join("\n\n");
        let outcomes: Vec<_> = notices.iter().map(TaskNotice::outcome).collect();
        let message_id = MessageId::new();
        if let Err(err) = live
            .session
            .send_task_notice(message_id, outcomes.clone(), text.clone())
            .await
        {
            warn!(workspace_id = %key.0, session_id = %key.1, "failed to deliver task notices: {err}");
            // Keep them, in order: the session refusing now says nothing about
            // later, and dropping them would lose the only record that those
            // tasks ended.
            state.pending_notices.extend(notices);
            return false;
        }
        let notice = TaskNoticeMessage::new(message_id, outcomes, text);
        if let Some(attachment) = &state.attached {
            let _ = attachment
                .tx
                .send(RelayEvent::TaskNotice(Box::new(notice.clone())));
        }
        let EntryPhase::Live(live) = &mut state.phase else {
            unreachable!("phase was Live above and the lock was never released");
        };
        live.turn_running = true;
        live.unsettled_user_message = Some((TurnId::from(message_id), Message::TaskNotice(notice)));
        true
    }

    /// Build a `LiveState` around a freshly opened session and start its event
    /// pipeline (pump + forwarder).
    fn make_live(
        &self,
        entry: &Arc<SessionEntry>,
        session: Session,
        provider_id: String,
        reasoning_effort: Option<String>,
        generation: u64,
    ) -> Box<LiveState> {
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
            self.opener.clone(),
            self.status_tx.clone(),
        );
        Box::new(LiveState {
            session,
            provider_id,
            reasoning_effort,
            generation,
            turn_running,
            snapshot,
            unsettled_user_message: None,
            pending_approvals: Vec::new(),
            log: EventLog::new(self.limits),
        })
    }

    async fn handle_task(
        state: &mut EntryState,
        key: &SessionKey,
        task: String,
        images: Vec<String>,
    ) -> CommandOutcome {
        if state.compacting {
            return CommandOutcome::NotIdle;
        }
        let EntryPhase::Live(live) = &mut state.phase else {
            return CommandOutcome::Ignored;
        };
        if live.turn_running
            || !live.pending_approvals.is_empty()
            || live.unsettled_user_message.is_some()
        {
            return CommandOutcome::NotIdle;
        }
        // This task becomes one user message that gets built twice: once inside
        // the session (its persisted history) and once here (the snapshot served
        // to attaching clients). Mint the id once so both copies — and every
        // later reference to them, like a rewind target — agree.
        let message_id = MessageId::new();
        // Send first, record after: a failed send must not leave a phantom
        // user message or a stuck running flag.
        if let Err(err) = live
            .session
            .send(message_id, task.clone(), images.clone())
            .await
        {
            return match err {
                SendCommandError::TurnAlreadyActive => CommandOutcome::NotIdle,
                _ => {
                    warn!(workspace_id = %key.0, session_id = %key.1, "failed to send task: {err}");
                    CommandOutcome::Ignored
                }
            };
        }
        live.turn_running = true;
        live.unsettled_user_message = Some((
            TurnId::from(message_id),
            Message::User(UserMessage::with_images(message_id, task, &images)),
        ));
        CommandOutcome::TaskAccepted { message_id }
    }

    /// Summarize the root thread and fold the result into the live view.
    ///
    /// The only command that lets go of the entry lock partway through: the
    /// summary is a 10–30 second round-trip, and holding the lock across it
    /// would block this session's attaches, aborts and every other command.
    /// What keeps that safe is two-layered — `compacting` stops anything from
    /// rewriting the thread or releasing the entry meanwhile, and the storage
    /// commit compare-and-swaps on the message count for the paths a flag
    /// cannot cover (process exit, eviction, a deleted session).
    async fn handle_compact(
        &self,
        entry: &Arc<SessionEntry>,
        mut guard: EntryGuard,
        key: &SessionKey,
        instructions: String,
    ) -> CommandOutcome {
        let (provider_id, reasoning_effort, generation) = {
            let state = &mut *guard;
            if state.compacting {
                return CommandOutcome::NotIdle;
            }
            let EntryPhase::Live(live) = &state.phase else {
                return CommandOutcome::Ignored;
            };
            if live.turn_running
                || !live.pending_approvals.is_empty()
                || live.unsettled_user_message.is_some()
            {
                return CommandOutcome::NotIdle;
            }
            if live.snapshot.is_empty() {
                return CommandOutcome::CompactionEmpty;
            }
            state.compacting = true;
            (
                live.provider_id.clone(),
                live.reasoning_effort.clone(),
                live.generation,
            )
        };
        push_snapshot(&guard);
        drop(guard);

        let result = self
            .opener
            .compact(
                key,
                &provider_id,
                reasoning_effort.as_deref(),
                &instructions,
            )
            .await;

        let mut guard = entry.inner.clone().lock_owned().await;
        guard.compacting = false;
        let outcome = match result {
            Ok(compacted) => {
                // The entry may have been rebuilt or torn down while the lock
                // was free. The write is safe either way — the commit's
                // compare-and-swap decided that — but this in-memory view is
                // only the right one to update if it is still the same one.
                if let EntryPhase::Live(live) = &mut guard.phase
                    && live.generation == generation
                {
                    live.snapshot.push(compacted.command);
                    live.snapshot.push(compacted.outcome);
                }
                CommandOutcome::Compacted {
                    applied: compacted.applied,
                }
            }
            Err(CompactError::Stale) => CommandOutcome::CompactionAbandoned {
                stale: true,
                reason: "the conversation changed while it was being summarized".to_string(),
            },
            Err(CompactError::Empty) => CommandOutcome::CompactionEmpty,
            Err(CompactError::InvalidHistory(reason)) => {
                warn!(workspace_id = %key.0, session_id = %key.1, "compaction found an invalid history: {reason}");
                CommandOutcome::CompactionAbandoned {
                    stale: false,
                    reason,
                }
            }
            Err(CompactError::Storage(reason)) => {
                warn!(workspace_id = %key.0, session_id = %key.1, "compaction failed to persist: {reason}");
                CommandOutcome::CompactionAbandoned {
                    stale: false,
                    reason,
                }
            }
        };
        push_snapshot(&guard);
        // The client may have disconnected during the round-trip, and the
        // release that would normally follow was held off by `compacting`.
        if let Some(release) = Self::maybe_release(&self.entries, entry, &mut guard).await {
            drop(guard);
            tokio::spawn(release);
        }
        outcome
    }

    /// Give up on an entry that can no longer serve: tell the attached client so
    /// it re-attaches (the connection layer does that transparently) and drop
    /// the slot so the next open starts from the persisted state. Used for the
    /// failures a rewind can hit *after* its truncation has committed — the
    /// client's view is stale from that moment on, and a fresh attach is the
    /// same route a crash would have forced anyway.
    fn abandon(entries: &Entries, entry: &Arc<SessionEntry>, state: &mut EntryState) {
        let release = Self::begin_release(entries, entry, state, Shutdown::abort(), true);
        tokio::spawn(release);
    }

    /// Discard the target message and everything after it, then start the
    /// replacement turn.
    ///
    /// Runs entirely under the entry lock: the idle check, the shutdown, the
    /// truncation, the rebuild and the new task have to be one step, or a
    /// command landing in the middle would drive a session that is halfway
    /// through being replaced.
    async fn handle_rewind(
        &self,
        entry: &Arc<SessionEntry>,
        state: &mut EntryState,
        key: &SessionKey,
        target: MessageId,
        task: String,
        images: Vec<String>,
    ) -> CommandOutcome {
        let permission_mode = state.permission_mode.clone();
        let background = self.ensure_background(entry, state).await;
        let compacting = state.compacting;
        let (provider_id, reasoning_effort, generation, previous_snapshot) = {
            let EntryPhase::Live(live) = &mut state.phase else {
                return CommandOutcome::Ignored;
            };
            // A rewind rewrites the very history a running compaction is
            // summarizing, and rebuilds the runtime under it.
            if compacting || live.turn_running || !live.pending_approvals.is_empty() {
                warn!(workspace_id = %key.0, session_id = %key.1, "ignoring rewind while the session is busy");
                return CommandOutcome::NotIdle;
            }
            // Stop the runtime before touching the persisted state: the
            // rebuild below opens a second runtime over the same session, and
            // the two must not overlap on it.
            //
            // This used to carry a second job — a sub-agent could reply before
            // saving, so "the turn settled" did not mean "no agent is still
            // writing". Replies are now sent only after checkpointing, so the
            // shutdown is here for the rebuild alone.
            live.session.shutdown(Shutdown::graceful_unbounded()).await;
            (
                live.provider_id.clone(),
                live.reasoning_effort.clone(),
                live.generation + 1,
                std::mem::take(&mut live.snapshot),
            )
        };

        let truncated = self.opener.rewind(key, target).await;

        // The runtime is gone either way, so it has to be rebuilt before the
        // entry can serve anything again — including on the failure path, where
        // the session is otherwise untouched and should carry on as before.
        let session = match self
            .opener
            .open(
                key,
                &provider_id,
                reasoning_effort.clone(),
                permission_mode,
                HashMap::new(),
                background,
            )
            .await
        {
            Ok(session) => session,
            Err(err) => {
                Self::abandon(&self.entries, entry, state);
                return CommandOutcome::OpenFailed(err);
            }
        };
        let mut replacement =
            self.make_live(entry, session, provider_id, reasoning_effort, generation);

        let messages = match truncated {
            Ok(messages) => {
                replacement.snapshot = messages.clone();
                messages
            }
            Err(err) => {
                // The truncation is all-or-nothing, so the history the entry
                // was serving is still current.
                replacement.snapshot = previous_snapshot;
                state.phase = EntryPhase::Live(replacement);
                warn!(workspace_id = %key.0, session_id = %key.1, "rewind rejected: {err}");
                return match err {
                    RewindError::TargetNotFound => CommandOutcome::RewindTargetNotFound,
                    RewindError::ThreadBusy { .. } => CommandOutcome::NotIdle,
                    RewindError::HistoryNotContiguous { .. } | RewindError::Persistence(_) => {
                        CommandOutcome::PersistenceFailed(err.to_string())
                    }
                };
            }
        };
        state.phase = EntryPhase::Live(replacement);

        // From here the truncation is committed, so any failure leaves the
        // client looking at messages that no longer exist. Rather than invent a
        // second recovery route, fall into the one a crash would have forced.
        match Self::handle_task(state, key, task, images).await {
            CommandOutcome::TaskAccepted { message_id } => CommandOutcome::Rewound {
                message_id,
                messages,
            },
            _ => {
                error!(
                    workspace_id = %key.0,
                    session_id = %key.1,
                    "rewind truncated the session but its replacement turn could not start"
                );
                Self::abandon(&self.entries, entry, state);
                CommandOutcome::RewindNotStarted
            }
        }
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
        let background = self.ensure_background(entry, state).await;
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
                let reasoning_effort = pending.reasoning_effort.clone();
                let decisions = std::mem::take(&mut pending.decisions);
                match self
                    .opener
                    .open(
                        key,
                        &provider_id,
                        reasoning_effort.clone(),
                        state.permission_mode.clone(),
                        decisions,
                        background,
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
                        // Match the previous behavior: the gated open is
                        // dropped; a fresh OpenSession retries from scratch.
                        Self::abandon(&self.entries, entry, state);
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
        reasoning_effort: Option<String>,
    ) -> CommandOutcome {
        // The registry is the entry's, not the runtime's: the replacement
        // session adopts the very tasks the outgoing one started.
        let background = self.ensure_background(entry, state).await;
        // Not `Live` (stale/not-attached is caught earlier by `command`'s guard;
        // this is the non-`Live` phase): the dispatcher reads `Ignored` on the
        // `set_model` path as `SESSION_NOT_LIVE` (Decision 8).
        let permission_mode = state.permission_mode.clone();
        // The rebuild below replaces the whole `LiveState`, which a running
        // compaction is about to write its result into.
        if state.compacting {
            return CommandOutcome::NotIdle;
        }
        let EntryPhase::Live(live) = &mut state.phase else {
            return CommandOutcome::Ignored;
        };
        // Selecting the current model is a benign no-op: idempotent success.
        if live.provider_id == provider_id && live.reasoning_effort == reasoning_effort {
            return CommandOutcome::Unchanged;
        }
        // The session is rebuilt with a new RunConfig; only safe while idle.
        if live.turn_running {
            warn!(workspace_id = %key.0, session_id = %key.1, "ignoring set_model while a turn is running");
            return CommandOutcome::TurnRunning;
        }
        if live.provider_id != provider_id {
            return CommandOutcome::ModelLocked;
        }
        // Open the replacement before tearing down the current session, so a
        // failed open leaves the existing one intact. The old session is idle,
        // so its checkpoint is durable and the new open reads current state.
        match self
            .opener
            .open(
                key,
                &provider_id,
                reasoning_effort.clone(),
                // The same cell, so the rebuilt runtime keeps reading the
                // posture the session already had.
                permission_mode,
                HashMap::new(),
                background,
            )
            .await
        {
            Ok(session) => {
                if let Err(error) = self
                    .opener
                    .update_reasoning_effort(key, &provider_id, reasoning_effort.as_deref())
                    .await
                {
                    session.shutdown(Shutdown::abort()).await;
                    return CommandOutcome::PersistenceFailed(error);
                }
                let generation = live.generation + 1;
                let mut replacement = self.make_live(
                    entry,
                    session,
                    provider_id.clone(),
                    reasoning_effort.clone(),
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
        reasoning_effort: Option<String>,
        permission_mode: PermissionMode,
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
                // Only a fresh entry adopts the client's mode; anything
                // already initialized keeps the one it is running under.
                state.permission_mode.set(permission_mode);
                let background = self.ensure_background(&entry, state).await;
                match self
                    .opener
                    .open(
                        &key,
                        &provider_id,
                        reasoning_effort.clone(),
                        state.permission_mode.clone(),
                        HashMap::new(),
                        background,
                    )
                    .await
                {
                    Ok(session) => {
                        state.phase = EntryPhase::Live(self.make_live(
                            &entry,
                            session,
                            provider_id,
                            reasoning_effort.clone(),
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
                        // Don't wedge the key: close the half-built registry
                        // before a fresh attach reopens this archive.
                        Self::abandon(&self.entries, &entry, state);
                        return Err(AttachError::Open(err));
                    }
                }
            }

            let snapshot = compose_snapshot(
                &state.phase,
                state.permission_mode.get(),
                state.compacting,
                current_tasks(state),
            )
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
            self.opener.clear_unseen_outcome(&key).await;

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
            // Taken by value rather than through `state` below: it is the one
            // command that has to drop the guard partway through.
            let command = match command {
                SessionCommand::Compact { instructions } => {
                    return self.handle_compact(&entry, guard, &key, instructions).await;
                }
                other => other,
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
                SessionCommand::Rewind {
                    target,
                    task,
                    images,
                } => {
                    self.handle_rewind(&entry, state, &key, target, task, images)
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
                // No phase check: the cell is the runtime's own source of
                // truth, so writing it is meaningful whether the session is
                // Live, mid-turn, or still Pending on its gated open.
                SessionCommand::SetPermissionMode { mode } => {
                    state.permission_mode.set(mode);
                    info!(workspace_id = %key.0, session_id = %key.1, "permission mode set to {mode:?}");
                    CommandOutcome::Ok
                }
                SessionCommand::KillTask { task_id } => {
                    let Some(background) = state.background.clone() else {
                        return CommandOutcome::Ignored;
                    };
                    let Ok(parsed) = task_id.parse() else {
                        return CommandOutcome::Ignored;
                    };
                    match background.kill(&parsed).await {
                        // Killing an already-finished task is not an error:
                        // the list the user clicked from can be a moment stale.
                        Ok(Some(_)) => CommandOutcome::Ok,
                        Ok(None) => CommandOutcome::Ignored,
                        Err(error) => {
                            warn!(workspace_id = %key.0, session_id = %key.1, "failed to kill task: {error}");
                            CommandOutcome::Ignored
                        }
                    }
                }
                SessionCommand::Compact { .. } => unreachable!("taken by the guard above"),
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
            if let Some(release) = Self::maybe_release(&self.entries, &entry, state).await {
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
    ) -> Pin<Box<dyn Future<Output = DeleteOutcome> + Send + 'a>> {
        Box::pin(async move {
            // The same gate attach takes, so a delete and an open of the same
            // key serialize even when neither found anything live — the slot
            // this borrows is the only thing either of them can lock, and it is
            // where the delete tombstone goes.
            let (entry, mut guard) = self.lock_entry_for_attach(&key).await;
            let borrowed = matches!(guard.phase, EntryPhase::Uninitialized);
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
                Self::leave_entry_gate(&self.entries, &entry, state, borrowed);
                return DeleteOutcome::NotOwner;
            }
            if state.compacting {
                Self::leave_entry_gate(&self.entries, &entry, state, borrowed);
                return DeleteOutcome::NotIdle;
            }
            if let Some(attachment) = state.attached.take() {
                let _ = attachment.tx.send(RelayEvent::Evicted);
            }
            let done = self.begin_delete(&entry, state);
            drop(guard);
            // Only watching: the delete is the hub's own task, so a caller
            // that goes away cannot strand the tombstone.
            await_delete(&done).await
        })
    }

    fn fork<'a>(
        &'a self,
        source: SessionKey,
        cut: Option<MessageId>,
    ) -> Pin<Box<dyn Future<Output = ForkOutcome> + Send + 'a>> {
        Box::pin(async move {
            // Take the same gate as attach rather than checking for an entry and
            // proceeding without one: between "no entry" and the copy, an attach
            // could insert one, open a runtime and start a turn.
            let (entry, mut guard) = self.lock_entry_for_attach(&source).await;
            let borrowed = matches!(guard.phase, EntryPhase::Uninitialized);

            let gate = match &guard.phase {
                _ if guard.compacting => ForkGate::Busy,
                EntryPhase::Live(live) => {
                    if live.turn_running
                        || !live.pending_approvals.is_empty()
                        || live.unsettled_user_message.is_some()
                    {
                        ForkGate::Busy
                    } else {
                        ForkGate::Ready
                    }
                }
                EntryPhase::Pending(_) => ForkGate::Busy,
                _ => ForkGate::Cold,
            };

            let source_state = match gate {
                ForkGate::Ready => ForkSource::Live,
                ForkGate::Cold => ForkSource::Cold,
                ForkGate::Busy => {
                    Self::leave_entry_gate(&self.entries, &entry, &mut guard, borrowed);
                    return ForkOutcome::NotIdle;
                }
            };
            let cut = match cut {
                Some(cut) => ForkCut::At(cut),
                None => ForkCut::All,
            };

            let forked = self.opener.fork(&source, cut, source_state).await;
            Self::leave_entry_gate(&self.entries, &entry, &mut guard, borrowed);

            match forked {
                Ok(forked) => ForkOutcome::Forked(forked),
                // A thread parked mid-turn is exactly that now, live or not: a
                // turn is announced only once its content is stored, so there is
                // no longer a lagging write for this to be mistaken for. Same
                // refusal either way, and the same one the gate's own check
                // produces.
                Err(ForkError::ThreadBusy { .. } | ForkError::SourceNotIdle { .. }) => {
                    ForkOutcome::NotIdle
                }
                Err(err) => {
                    warn!(workspace_id = %source.0, session_id = %source.1, "fork rejected: {err}");
                    ForkOutcome::Failed(err)
                }
            }
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
                if let EntryPhase::Releasing { done } = &state.phase {
                    let mut done = done.clone();
                    drop(guard);
                    while !*done.borrow_and_update() {
                        if done.changed().await.is_err() {
                            break;
                        }
                    }
                    continue;
                }
                if let EntryPhase::Deleting { done } = &state.phase {
                    // Same reason as `Releasing`: the process must not exit
                    // while a delete is still killing process groups and
                    // removing files.
                    let done = done.clone();
                    drop(guard);
                    let _ = await_delete(&done).await;
                    continue;
                }
                if matches!(
                    state.phase,
                    EntryPhase::Released | EntryPhase::Uninitialized
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

    fn subscribe_status(&self) -> BoxStream<'static, SessionStatusEvent> {
        // Drop lag errors; a missed event is a freshness gap, not a fault.
        BroadcastStream::new(self.status_tx.subscribe())
            .filter_map(|event| async move { event.ok() })
            .boxed()
    }

    fn running_sessions<'a>(
        &'a self,
        workspace_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = HashSet<String>> + Send + 'a>> {
        Box::pin(async move {
            // Release the outer lock before awaiting each entry's own lock,
            // which can be held for a while (e.g. a stalled settle write).
            let candidates: Vec<Arc<SessionEntry>> = self
                .entries
                .lock()
                .expect("entries mutex poisoned")
                .iter()
                .filter(|(key, _)| key.0 == workspace_id)
                .map(|(_, entry)| entry.clone())
                .collect();
            let mut running = HashSet::new();
            for entry in candidates {
                let guard = entry.inner.clone().lock_owned().await;
                if matches!(&guard.phase, EntryPhase::Live(live) if live.turn_running) {
                    running.insert(entry.key.1.clone());
                }
            }
            running
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

/// Hand the attached client the current snapshot. Used where the session
/// changes outside the event stream, which is the one thing a client following
/// only events cannot see.
fn push_snapshot(state: &EntryState) {
    let Some(attachment) = &state.attached else {
        return;
    };
    if let Some(snapshot) = compose_snapshot(
        &state.phase,
        state.permission_mode.get(),
        state.compacting,
        current_tasks(state),
    ) {
        let _ = attachment.tx.send(RelayEvent::Snapshot(Box::new(snapshot)));
    }
}

/// The entry's current task overview, or an empty list before its registry
/// exists.
fn current_tasks(state: &EntryState) -> Arc<[TaskSummary]> {
    state
        .background
        .as_ref()
        .map(|background| background.summaries().borrow().clone())
        .unwrap_or_else(|| Arc::from(Vec::new().into_boxed_slice()))
}

/// Compose the attach-time snapshot for an entry. Pure over the entry state so
/// it is unit-testable.
fn compose_snapshot(
    phase: &EntryPhase,
    permission_mode: PermissionMode,
    compacting: bool,
    background_tasks: Arc<[TaskSummary]>,
) -> Option<SnapshotPayload> {
    match phase {
        EntryPhase::Live(live) => {
            let mut messages = live.snapshot.clone();
            messages.extend(
                live.unsettled_user_message
                    .iter()
                    .map(|(_, message)| message.clone()),
            );
            Some(SnapshotPayload {
                messages,
                pending_approvals: live.pending_approvals.clone(),
                provider_id: live.provider_id.clone(),
                reasoning_effort: live.reasoning_effort.clone(),
                permission_mode,
                turn_running: live.turn_running,
                compacting,
                background_tasks,
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
            reasoning_effort: pending.reasoning_effort.clone(),
            permission_mode,
            turn_running: false,
            compacting,
            background_tasks,
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
/// Watch a session's task registry for as long as its entry lives. Every
/// change to the overview means a task started or settled — which is exactly
/// when a notice may have appeared to deliver, and when the entry may have
/// become releasable.
///
/// Notices are drained into the entry rather than left in the registry so that
/// "has something to say" is one of the entry's own keepalive conditions,
/// checked under the same lock as everything else that decides a release.
fn spawn_notice_watcher(
    entries: Entries,
    entry: Arc<SessionEntry>,
    background: Arc<BackgroundProcesses>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        let mut rx = background.summaries();
        // Mark what the watch already holds as seen: building the registry
        // publishes once (seeding from the archive), and that is not a change
        // anyone attached could have missed — the attach snapshot carries it.
        rx.borrow_and_update();
        // The first pass reads the value the watch already holds, which the
        // attach snapshot carries too — so it drains notices (a task can
        // finish between the registry being built and this subscribing) but
        // pushes nothing. Only an actual change is news.
        let mut changed = false;
        loop {
            {
                // Entry lock first, then the registry's — the order every
                // other path takes.
                let mut guard = entry.inner.clone().lock_owned().await;
                if matches!(
                    guard.phase,
                    EntryPhase::Releasing { .. }
                        | EntryPhase::Deleting { .. }
                        | EntryPhase::Released
                ) {
                    return;
                }
                if changed && let Some(attachment) = &guard.attached {
                    let _ = attachment
                        .tx
                        .send(RelayEvent::BackgroundTasks(current_tasks(&guard)));
                }
                SessionHub::deliver_pending_notices(&mut guard, &entry.key).await;
                if let Some(release) = SessionHub::maybe_release(&entries, &entry, &mut guard).await
                {
                    drop(guard);
                    release.await;
                    return;
                }
            }
            if rx.changed().await.is_err() {
                return;
            }
            changed = true;
        }
    })
    .abort_handle()
}

fn spawn_event_pipeline(
    entries: Entries,
    entry: Arc<SessionEntry>,
    session: Session,
    root_name: String,
    generation: u64,
    opener: Arc<dyn SessionOpener>,
    status_tx: broadcast::Sender<SessionStatusEvent>,
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
        entries, entry, rx, root_name, generation, opener, status_tx,
    ));
}

/// Force the entry to drain and resync from the persisted state: used when
/// the in-memory event log can no longer be trusted (a lagged broadcast
/// receiver, or a checkpoint the database refused) or has grown past what it
/// may safely buffer (a runaway turn).
///
/// `mode` is the caller's read of how the runtime is doing, because that is
/// what decides whether waiting is a good idea. A log that lagged or overflowed
/// says nothing about the session itself, so an unbounded wait lets a healthy
/// turn reach its own checkpoint and the next attach read a current state. A
/// write that failed says the opposite, and a caller that waits unbounded on a
/// runtime it just declared broken can wait forever — with the key locked
/// behind it.
async fn force_resync(
    entries: &Entries,
    entry: &Arc<SessionEntry>,
    mut guard: EntryGuard,
    mode: Shutdown,
    reason: String,
) {
    error!(
        workspace_id = %entry.key.0,
        session_id = %entry.key.1,
        "{reason}; draining session to resync from the persisted state"
    );
    let release = SessionHub::begin_release(entries, entry, &mut guard, mode, true);
    drop(guard);
    release.await;
}

async fn run_forwarder(
    entries: Entries,
    entry: Arc<SessionEntry>,
    mut rx: mpsc::UnboundedReceiver<SessionStreamItem>,
    root_name: String,
    generation: u64,
    opener: Arc<dyn SessionOpener>,
    status_tx: broadcast::Sender<SessionStatusEvent>,
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
                    guard,
                    Shutdown::graceful_unbounded(),
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
                let turn_id = event.turn_id;
                let wire = WireEvent::from_session_event(event, &root_name);
                // Restart-resume starts work without a command flipping the
                // flag; the turn's first event does.
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
                if let WireEvent::PersistFailed { message, .. } = &wire {
                    // The turn's content never reached the database, so the
                    // in-memory view is now a claim nothing can back. Drop it
                    // and let the client rebuild from what is actually stored —
                    // the same route a lagged stream takes.
                    //
                    // On a deadline, though. One way to reach here is a turn
                    // that gave up on a sub-agent wedged mid-write, and waiting
                    // out an agent that is already stuck is how the entry never
                    // comes back at all.
                    let reason = format!("checkpoint write failed: {message}");
                    force_resync(
                        &entries,
                        &entry,
                        guard,
                        Shutdown::graceful_then_abort(BROKEN_RUNTIME_GRACE),
                        reason,
                    )
                    .await;
                    return;
                }
                if event_settles_turn(&wire, &root_name) {
                    // `suspended` is moved by the match below; read before it.
                    let awaits_approval = suspended.is_some();
                    match suspended {
                        Some(approval) => live.pending_approvals.push(approval),
                        // Any other settlement is final: the turn those
                        // approvals belonged to is over, and a decision for
                        // them has no thread left to wake. Kept around, they
                        // would hold admission and fork at `NotIdle` forever.
                        None => live.pending_approvals.clear(),
                    }
                    live.turn_running = fold_settled_turn(
                        &mut live.snapshot,
                        &mut live.unsettled_user_message,
                        &mut live.log,
                        &root_name,
                        turn_id,
                    );
                    // A task that finished while this turn ran gets the next
                    // one. Checked before the bookkeeping below, so a session
                    // that is about to keep working is not recorded as having
                    // finished unattended.
                    let delivered = SessionHub::deliver_pending_notices(state, &entry.key).await;
                    let EntryPhase::Live(live) = &mut state.phase else {
                        return;
                    };
                    let awaits_approval = awaits_approval && !delivered;
                    // Suspensions awaiting approval already have their own
                    // indicator. Still under `guard`, so no attach can land
                    // between "nobody's here" and "we recorded that".
                    if !live.turn_running && !awaits_approval && state.attached.is_none() {
                        let outcome = unseen_outcome_for(&wire);
                        opener.mark_unseen_outcome(&entry.key, outcome).await;
                        let _ = status_tx.send(SessionStatusEvent {
                            workspace_id: entry.key.0.clone(),
                            session_id: entry.key.1.clone(),
                            outcome,
                        });
                    }
                    if let Some(release) = SessionHub::maybe_release(&entries, &entry, state).await
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
                    force_resync(
                        &entries,
                        &entry,
                        guard,
                        Shutdown::graceful_unbounded(),
                        reason,
                    )
                    .await;
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
#[path = "hub_tests/mod.rs"]
mod tests;
