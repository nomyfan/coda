//! Session-scoped background task registry (design: docs/design/background-tasks.md).
//!
//! `BackgroundProcesses` owns the lifecycle of background work independently
//! of any turn: tasks are started via [`BackgroundProcesses::spawn_with`],
//! observed via incremental reads and a summaries watch, torn down via
//! [`kill`](BackgroundProcesses::kill) / [`shutdown`](BackgroundProcesses::shutdown),
//! and their completions accumulate as [`TaskNotice`]s until a caller drains
//! them for delivery.
//!
//! The registry is generic over what a task *runs* (a boxed future given a
//! [`TaskCtx`]); the process-backed `spawn` builds on this same engine. The
//! future seam stays public: cross-crate lifecycle tests drive fake tasks
//! through it.

mod archive_dir;
mod disk_tail;
mod manifest;
mod quota;
mod task_archive;
mod task_id;

pub use archive_dir::{ArchiveDir, ArchiveError, ArchiveFileName};
pub use disk_tail::{DiskTail, OutputChunk};
pub use manifest::{ExpireReason, OutputDisposition, StreamManifest, TaskOutputManifest};
pub use quota::{
    ArchiveInventory, ExpirationFact, InventoryIssue, QuotaError, QuotaReservation, ReserveOutcome,
    RetainedIndexEntry, SESSION_QUOTA_BYTES, SessionQuota, scan_inventory,
};
pub use task_archive::{
    DEFAULT_STREAM_CAPACITY, TaskArchive, TaskCommitGuard, TaskOutputFiles, TaskPersistentState,
    TaskRecord,
};
pub use task_id::{InvalidTaskId, TaskId};

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use coda_core::llm::TaskNoticeKey;
use coda_core::tool::CancellationToken;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use crate::process::{GroupedChild, PIPE_DRAIN_TIMEOUT};

/// Concurrent `Running` tasks per session.
const MAX_RUNNING: usize = 16;
/// Terminal tasks retained for reads; beyond this the oldest is reclaimed.
const MAX_TERMINAL: usize = 32;
/// Full notices (with output tail) buffered; older ones degrade into the
/// overflow aggregate.
const MAX_FULL_NOTICES: usize = 64;
/// (id, status) pairs the overflow aggregate holds; beyond this only a count.
const MAX_OVERFLOW_ENTRIES: usize = 256;
/// Output tail carried by one full notice.
const NOTICE_TAIL_LIMIT: usize = 4096;
/// Bytes returned per stream by one `read` (128 KiB); the cursor advances only
/// over what is actually returned, so a large backlog drains across calls.
const READ_CHUNK_LIMIT: usize = 128 * 1024;

/// Caller-supplied identity of a task, echoed in summaries and notices.
#[derive(Clone, Debug)]
pub struct TaskMeta {
    pub command: String,
    pub description: String,
    pub agent_name: String,
}

/// Where a task stands. Terminal states are committed exactly once, by the
/// task's monitor (the single writer).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TaskStatus {
    Running,
    Exited {
        code: Option<i32>,
        at: jiff::Timestamp,
    },
    Killed {
        at: jiff::Timestamp,
    },
    /// The task's output spool failed irrecoverably (a ring append/read or a
    /// terminal manifest save). The process outcome is subsumed by this state
    /// so a spool failure is never misreported as a clean exit.
    Failed {
        message: String,
        at: jiff::Timestamp,
    },
    /// A `Running` task left behind by a crash, recovered at reopen. The
    /// process is gone; only the (possibly partial) output remains readable.
    Interrupted {
        at: jiff::Timestamp,
    },
}

impl TaskStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, TaskStatus::Running)
    }

    /// Terminal time of a settled task, `None` while `Running`.
    pub fn terminal_at(&self) -> Option<jiff::Timestamp> {
        match self {
            TaskStatus::Running => None,
            TaskStatus::Exited { at, .. }
            | TaskStatus::Killed { at }
            | TaskStatus::Failed { at, .. }
            | TaskStatus::Interrupted { at } => Some(*at),
        }
    }

    /// Model-facing one-line rendering.
    pub fn describe(&self) -> String {
        match self {
            TaskStatus::Running => "running".into(),
            TaskStatus::Exited {
                code: Some(code), ..
            } => format!("exited with code {code}"),
            TaskStatus::Exited { code: None, .. } => "exited (unknown exit code)".into(),
            TaskStatus::Killed { .. } => "killed".into(),
            TaskStatus::Failed { message, .. } => format!("failed: {message}"),
            TaskStatus::Interrupted { .. } => "interrupted (server restarted)".into(),
        }
    }
}

/// One row of the registry's live overview (dashboard / keepalive signal).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub command: String,
    pub description: String,
    pub agent_name: String,
    pub status: TaskStatus,
    pub started_at: jiff::Timestamp,
}

/// One terminal fact carried inside an overflow aggregate: either a completion
/// or an output-expiration, so the aggregate never has to fake a task id for a
/// fact it cannot express.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TaskNoticeFact {
    Completed { id: TaskId, status: TaskStatus },
    OutputExpired { id: TaskId, reason: ExpireReason },
}

impl TaskNoticeFact {
    /// The stable dedupe key for this fact.
    pub fn key(&self) -> TaskNoticeKey {
        match self {
            TaskNoticeFact::Completed { id, .. } => TaskNoticeKey::Completed {
                task_id: id.as_str().to_owned(),
            },
            TaskNoticeFact::OutputExpired { id, .. } => TaskNoticeKey::OutputExpired {
                task_id: id.as_str().to_owned(),
            },
        }
    }

    fn describe(&self) -> String {
        match self {
            TaskNoticeFact::Completed { id, status } => format!("{id}: {}", status.describe()),
            TaskNoticeFact::OutputExpired { id, .. } => {
                format!("{id}: output expired (session output quota)")
            }
        }
    }
}

/// A notice awaiting delivery. `Task` carries a bounded output tail and the
/// storage-level overwrite totals; `OutputExpired` is a separate later fact for
/// a task whose retained output the quota evicted; `Overflow` aggregates facts
/// evicted from the full-notice window so the terminal/expiration *fact*
/// survives even under a flood.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TaskNotice {
    Task {
        id: TaskId,
        command: String,
        description: String,
        status: TaskStatus,
        output_tail: String,
        /// Bytes this stream overwrote due to ring capacity, regardless of
        /// whether the model had already read them (a storage fact).
        #[serde(default)]
        stdout_overwritten: u64,
        #[serde(default)]
        stderr_overwritten: u64,
    },
    OutputExpired {
        id: TaskId,
        expired_at: jiff::Timestamp,
        reason: ExpireReason,
    },
    Overflow {
        /// Stable id minted when the aggregate is first created; preserved across
        /// merge and NoticeStore round-trips so the whole batch dedupes as one.
        #[serde(default)]
        batch_id: String,
        dropped: Vec<TaskNoticeFact>,
        uncounted: u64,
    },
}

impl TaskNotice {
    /// Every stable fact key this notice covers. `Completed` and `OutputExpired`
    /// of the same task produce *different* keys.
    pub fn keys(&self) -> Vec<TaskNoticeKey> {
        match self {
            TaskNotice::Task { id, .. } => vec![TaskNoticeKey::Completed {
                task_id: id.as_str().to_owned(),
            }],
            TaskNotice::OutputExpired { id, .. } => vec![TaskNoticeKey::OutputExpired {
                task_id: id.as_str().to_owned(),
            }],
            TaskNotice::Overflow {
                batch_id, dropped, ..
            } => {
                let mut keys = vec![TaskNoticeKey::OverflowBatch {
                    batch_id: batch_id.clone(),
                }];
                keys.extend(dropped.iter().map(TaskNoticeFact::key));
                keys
            }
        }
    }

    /// The text of the user-turn message that delivers this notice — what the
    /// model (and the user, as a notice card) reads.
    pub fn render(&self) -> String {
        match self {
            TaskNotice::Task {
                id,
                command,
                description,
                status,
                output_tail,
                stdout_overwritten,
                stderr_overwritten,
            } => {
                let mut text = format!("Background task {id} finished: {}.", status.describe());
                text.push_str(&format!("\nCommand: {command}"));
                if !description.is_empty() {
                    text.push_str(&format!("\nDescription: {description}"));
                }
                let overwritten = stdout_overwritten + stderr_overwritten;
                if overwritten > 0 {
                    text.push_str(&format!(
                        "\n({overwritten} bytes of earlier output were overwritten as the task ran)"
                    ));
                }
                if !output_tail.is_empty() {
                    text.push_str(&format!("\nOutput tail:\n{output_tail}"));
                } else {
                    text.push_str("\n(no output)");
                }
                text
            }
            TaskNotice::OutputExpired { id, .. } => {
                format!(
                    "Background task {id}'s retained output was evicted to reclaim \
                     the session output quota; it is no longer readable."
                )
            }
            TaskNotice::Overflow {
                dropped, uncounted, ..
            } => {
                let total = dropped.len() as u64 + uncounted;
                let mut text = format!(
                    "{total} more background task event(s) occurred while notices were capped:"
                );
                for fact in dropped {
                    text.push_str(&format!("\n- {}", fact.describe()));
                }
                if *uncounted > 0 {
                    text.push_str(&format!("\n…and {uncounted} more (details dropped)."));
                }
                text
            }
        }
    }
}

/// Result of an incremental read: output produced since the previous read.
/// `*_lost` count bytes that were already dropped from the tail buffer before
/// this read could observe them.
#[derive(Debug)]
pub struct TaskRead {
    pub status: TaskStatus,
    pub stdout: String,
    pub stderr: String,
    pub stdout_lost: u64,
    pub stderr_lost: u64,
    /// A storage-level note (output consumed or quota-expired), separate from
    /// the streams so it is never mistaken for task output.
    pub note: Option<String>,
}

/// How the task's work future resolved. The process-backed runner reports
/// `Killed` when it tore the process group down in response to cancellation.
#[derive(Debug)]
pub enum TaskExit {
    Exited {
        code: Option<i32>,
    },
    Killed,
    /// The output spool failed; the process was torn down and this cause is
    /// carried to the terminal commit as [`TaskStatus::Failed`].
    Failed {
        message: String,
    },
}

/// A live task entry: the archive-backed record plus its cancellation token.
/// Output bytes live only in the record's ring files, never in memory here.
struct TaskEntry {
    record: Arc<TaskRecord>,
    /// Independent of any turn token: only `kill`/`shutdown` cancel it.
    cancel: CancellationToken,
}

impl TaskEntry {
    fn id(&self) -> &TaskId {
        self.record.id()
    }

    fn summary(&self, status: TaskStatus) -> TaskSummary {
        TaskSummary {
            id: self.record.id().as_str().to_owned(),
            command: self.record.meta().command.clone(),
            description: self.record.meta().description.clone(),
            agent_name: self.record.meta().agent_name.clone(),
            status,
            started_at: self.record.started_at(),
        }
    }
}

/// Handle a task's work future uses to stream output into its ring files.
/// Appends take only the per-stream `DiskTail` lock — never the registry or
/// commit lock — so a chatty task never contends with reads or bookkeeping.
#[derive(Clone)]
pub struct TaskCtx {
    record: Arc<TaskRecord>,
    cancel: CancellationToken,
}

impl TaskCtx {
    /// Cancellation requested via `kill`/`shutdown`. Process-backed work kills
    /// its group and resolves to [`TaskExit::Killed`]; fake work just races it.
    pub fn cancelled(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub async fn append_stdout(&self, data: &[u8]) -> std::io::Result<()> {
        self.record.files().stdout.append(data).await
    }

    pub async fn append_stderr(&self, data: &[u8]) -> std::io::Result<()> {
        self.record.files().stderr.append(data).await
    }
}

/// The overflow aggregate slot: a stable batch id plus the facts and a bare
/// count. Never dropped, unlike the full notices feeding it.
struct OverflowSlot {
    batch_id: String,
    dropped: Vec<TaskNoticeFact>,
    uncounted: u64,
}

struct RegistryState {
    tasks: HashMap<TaskId, Arc<TaskEntry>>,
    /// Redundant indexes so everything below is answerable while holding this
    /// lock alone — a task's ring/commit locks are never taken under it.
    running_count: usize,
    summaries: HashMap<TaskId, TaskSummary>,
    terminal_order: VecDeque<TaskId>,
    monitors: HashMap<TaskId, JoinHandle<()>>,
    notices: Vec<TaskNotice>,
    overflow: Option<OverflowSlot>,
    closed: bool,
    summaries_tx: watch::Sender<Arc<[TaskSummary]>>,
}

impl RegistryState {
    /// Whether another task may start: rejects once closed or at the running
    /// limit. Checked before any process is spawned, so rejection has no
    /// side effects.
    fn check_capacity(&self) -> std::io::Result<()> {
        if self.closed {
            return Err(std::io::Error::other("background registry is shut down"));
        }
        if self.running_count >= MAX_RUNNING {
            return Err(std::io::Error::other(format!(
                "too many running background tasks (limit {MAX_RUNNING})"
            )));
        }
        Ok(())
    }

    /// Recompute and publish the summaries snapshot. Per the terminal-commit
    /// protocol this must be the *last* mutation of a commit: when a watcher
    /// observes zero running tasks, the matching notice is already enqueued.
    fn publish(&self) {
        let mut all: Vec<TaskSummary> = self.summaries.values().cloned().collect();
        all.sort_by(|a, b| (a.started_at, &a.id).cmp(&(b.started_at, &b.id)));
        self.summaries_tx.send_replace(all.into());
    }

    /// Fold facts into the aggregate slot, spilling into the bare count beyond
    /// its capacity. Mints a stable batch id the first time the slot is used.
    fn merge_overflow(&mut self, facts: Vec<TaskNoticeFact>, uncounted: u64) {
        let slot = self.overflow.get_or_insert_with(|| OverflowSlot {
            batch_id: uuid::Uuid::new_v4().simple().to_string(),
            dropped: Vec::new(),
            uncounted: 0,
        });
        slot.uncounted += uncounted;
        for fact in facts {
            if slot.dropped.len() < MAX_OVERFLOW_ENTRIES {
                slot.dropped.push(fact);
            } else {
                slot.uncounted += 1;
            }
        }
    }

    /// Preserve a restored aggregate's batch id so it dedupes as one batch.
    fn merge_overflow_batch(
        &mut self,
        batch_id: String,
        facts: Vec<TaskNoticeFact>,
        uncounted: u64,
    ) {
        if self.overflow.is_none() {
            self.overflow = Some(OverflowSlot {
                batch_id,
                dropped: Vec::new(),
                uncounted: 0,
            });
        }
        self.merge_overflow(facts, uncounted);
    }

    /// `notices` holds only full `Task`/`OutputExpired` entries (aggregates live
    /// in the `overflow` slot); the oldest degrades to a fact on overflow.
    fn push_notice(&mut self, notice: TaskNotice) {
        self.notices.push(notice);
        if self.notices.len() > MAX_FULL_NOTICES {
            let demoted = self.notices.remove(0);
            if let Some(fact) = notice_into_fact(demoted) {
                self.merge_overflow(vec![fact], 0);
            }
        }
    }
}

/// Demote a full notice to the fact the overflow aggregate carries.
fn notice_into_fact(notice: TaskNotice) -> Option<TaskNoticeFact> {
    match notice {
        TaskNotice::Task { id, status, .. } => Some(TaskNoticeFact::Completed { id, status }),
        TaskNotice::OutputExpired { id, reason, .. } => {
            Some(TaskNoticeFact::OutputExpired { id, reason })
        }
        TaskNotice::Overflow { .. } => None,
    }
}

/// The disk-backed store behind a live registry: the session archive plus its
/// quota. `temp` is `Some` only for a self-owned (temporary) registry, whose
/// output directory is deleted when the registry drops.
struct Backend {
    archive: Arc<TaskArchive>,
    quota: SessionQuota,
    /// Held only for its `Drop`: deletes the temporary output directory when the
    /// registry drops. `None` for a session-backed registry (output persists).
    #[allow(dead_code)]
    temp: Option<tempfile::TempDir>,
}

/// Storage backing: enabled (archive + quota) or disabled (the session archive
/// root could not be opened; the conversation still works, background is off).
#[derive(Clone)]
enum Store {
    Enabled(Arc<Backend>),
    Disabled(Arc<str>),
}

/// Session-scoped background task registry. The owner (hub entry, or the
/// `Session` itself when self-built) is responsible for calling
/// [`shutdown`](Self::shutdown) per the ownership rules in the design doc.
pub struct BackgroundProcesses {
    inner: Arc<Mutex<RegistryState>>,
    summaries_rx: watch::Receiver<Arc<[TaskSummary]>>,
    /// Serializes concurrent `shutdown` calls: the join-before-drain barrier
    /// must hold for every caller, not just the one that drains the monitor
    /// handles first.
    shutdown_gate: Mutex<()>,
    store: Store,
}

impl Default for BackgroundProcesses {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundProcesses {
    fn with_store(store: Store) -> Self {
        let (summaries_tx, summaries_rx) = watch::channel(Arc::from(Vec::new().into_boxed_slice()));
        BackgroundProcesses {
            inner: Arc::new(Mutex::new(RegistryState {
                tasks: HashMap::new(),
                running_count: 0,
                summaries: HashMap::new(),
                terminal_order: VecDeque::new(),
                monitors: HashMap::new(),
                notices: Vec::new(),
                overflow: None,
                closed: false,
                summaries_tx,
            })),
            summaries_rx,
            shutdown_gate: Mutex::new(()),
            store,
        }
    }

    /// A self-owned registry backed by a fresh temporary directory whose output
    /// is deleted when the registry drops. The default backing for a standalone
    /// `Session`.
    pub fn temporary() -> Self {
        match Self::try_temporary() {
            Ok(reg) => reg,
            Err(e) => Self::with_store(Store::Disabled(
                format!("could not create temporary background archive: {e}").into(),
            )),
        }
    }

    fn try_temporary() -> std::io::Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = ArchiveDir::open_or_create_root(&temp.path().join("background/tasks"))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let archive = Arc::new(TaskArchive::new(root));
        let quota = SessionQuota::from_inventory(
            &ArchiveInventory::default(),
            SESSION_QUOTA_BYTES,
            archive.clone(),
        );
        Ok(Self::with_store(Store::Enabled(Arc::new(Backend {
            archive,
            quota,
            temp: Some(temp),
        }))))
    }

    /// Equivalent to [`temporary`](Self::temporary); the historical name.
    pub fn new() -> Self {
        Self::temporary()
    }

    /// A hub-owned registry backed by a session archive directory. Runs the
    /// session-local inventory to rebuild the quota and corruption blocker,
    /// seeds the live overview from recent terminal summaries, and converts any
    /// crash-`Running` task that passes validation to `Interrupted`. Output is
    /// **not** deleted on shutdown.
    pub async fn session_backed(archive_dir: ArchiveDir) -> Self {
        let archive = Arc::new(TaskArchive::new(archive_dir));
        let scan_root = archive.root().clone();
        let inventory = match tokio::task::spawn_blocking(move || scan_inventory(&scan_root)).await
        {
            Ok(Ok(inv)) => inv,
            Ok(Err(e)) => return Self::disabled_from(e.to_string()),
            Err(e) => return Self::disabled_from(format!("inventory worker failed: {e}")),
        };
        let quota = SessionQuota::from_inventory(&inventory, SESSION_QUOTA_BYTES, archive.clone());
        let reg = Self::with_store(Store::Enabled(Arc::new(Backend {
            archive: archive.clone(),
            quota,
            temp: None,
        })));
        reg.seed_from_inventory(&archive, inventory).await;
        reg
    }

    /// A disabled registry: the archive root could not be opened. Spawn/read/
    /// kill return a clear error; summaries are empty; the conversation is
    /// otherwise unaffected.
    pub fn disabled(error: ArchiveError) -> Self {
        Self::disabled_from(error.to_string())
    }

    fn disabled_from(error: String) -> Self {
        Self::with_store(Store::Disabled(error.into()))
    }

    /// Seed the live overview from an inventory scan and convert recoverable
    /// crash-`Running` tasks to `Interrupted` (durably, one commit each).
    async fn seed_from_inventory(&self, archive: &TaskArchive, inventory: ArchiveInventory) {
        let mut inner = self.inner.lock().await;
        for summary in inventory.recent_terminal {
            if let Ok(id) = summary.id.parse::<TaskId>() {
                inner.summaries.insert(id, summary);
            }
        }
        drop(inner);

        for id in inventory.recoverable_running {
            if let Ok(Some(record)) = archive.open(&id).await {
                let mut guard = record.lock_commit().await;
                let mut candidate = guard.current().clone();
                candidate.status = TaskStatus::Interrupted {
                    at: jiff::Timestamp::now(),
                };
                if guard.commit(candidate).await.is_ok() {
                    let status = guard.current().status.clone();
                    drop(guard);
                    let mut inner = self.inner.lock().await;
                    inner.summaries.insert(
                        id.clone(),
                        TaskSummary {
                            id: id.as_str().to_owned(),
                            command: record.meta().command.clone(),
                            description: record.meta().description.clone(),
                            agent_name: record.meta().agent_name.clone(),
                            status,
                            started_at: record.started_at(),
                        },
                    );
                }
            }
        }
        self.inner.lock().await.publish();
    }

    fn backend(&self) -> std::io::Result<Arc<Backend>> {
        match &self.store {
            Store::Enabled(b) => Ok(b.clone()),
            Store::Disabled(e) => Err(std::io::Error::other(e.to_string())),
        }
    }

    /// Start `cmd` as a background process task in its own sentinel-pinned
    /// process group. Rejection (closed / running limit / disabled / quota) has
    /// no side effects; only `kill`/`shutdown` terminate a started task.
    pub async fn spawn(&self, mut cmd: Command, meta: TaskMeta) -> std::io::Result<TaskId> {
        // The group is spawned first so a spawn failure has no archive residue;
        // if the reservation/create fails, the unused closure drops the group
        // (its Drop kills it).
        let group = GroupedChild::spawn(&mut cmd)?;
        self.register_task(meta, move |ctx| run_process(group, ctx))
            .await
    }

    /// Start `work` as a background task. The task is visible in the summaries
    /// (and thus to keepalive watchers) before the id is returned. Fails when
    /// closed, at `MAX_RUNNING`, disabled, or the quota is blocked.
    pub async fn spawn_with<F, Fut>(&self, meta: TaskMeta, work: F) -> std::io::Result<TaskId>
    where
        F: FnOnce(TaskCtx) -> Fut,
        Fut: Future<Output = TaskExit> + Send + 'static,
    {
        self.register_task(meta, work).await
    }

    /// Reserve quota, create the archive record, and register the task. The
    /// registry lock is held across the whole sequence so the capacity check
    /// and the registration are atomic (a concurrent spawn cannot exceed the
    /// limit); this is deadlock-safe because no path holds a per-task commit or
    /// quota lock while waiting for the registry lock. On any failure before
    /// registration nothing is published, and the unused `work` closure drops
    /// its process group.
    async fn register_task<F, Fut>(&self, meta: TaskMeta, work: F) -> std::io::Result<TaskId>
    where
        F: FnOnce(TaskCtx) -> Fut,
        Fut: Future<Output = TaskExit> + Send + 'static,
    {
        let backend = self.backend()?;
        let mut inner = self.inner.lock().await;
        inner.check_capacity()?;

        let outcome = backend
            .quota
            .reserve_for_create(2 * DEFAULT_STREAM_CAPACITY)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let id = TaskId::new();
        let record = backend
            .archive
            .create(&id, &meta)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        outcome.reservation.commit();

        // Enqueue any expirations the reservation caused (quota never touches
        // the notice queue itself).
        for fact in outcome.expirations {
            inner.push_notice(TaskNotice::OutputExpired {
                id: fact.id,
                expired_at: fact.expired_at,
                reason: fact.reason,
            });
        }
        let cancel = CancellationToken::new();
        let entry = Arc::new(TaskEntry {
            record: record.clone(),
            cancel: cancel.clone(),
        });
        let fut = work(TaskCtx { record, cancel });
        let monitor = tokio::spawn(monitor_task(
            self.inner.clone(),
            backend.clone(),
            entry.clone(),
            fut,
        ));
        inner.tasks.insert(id.clone(), entry.clone());
        inner.running_count += 1;
        inner
            .summaries
            .insert(id.clone(), entry.summary(TaskStatus::Running));
        inner.monitors.insert(id.clone(), monitor);
        inner.publish();
        Ok(id)
    }

    /// Incremental read: output since the previous read plus current status.
    /// `Ok(None)` for an unknown id; `Err` for a disabled/corrupt archive. The
    /// cursor is persisted before any bytes are returned, so a failed save
    /// yields an error rather than silently advancing the cursor.
    pub async fn read(&self, id: &TaskId) -> Result<Option<TaskRead>, TaskAccessError> {
        let backend = self.enabled()?;
        let Some(record) = backend
            .archive
            .open(id)
            .await
            .map_err(TaskAccessError::from)?
        else {
            return Ok(None);
        };

        let mut guard = record.lock_commit().await;
        let state = guard.current().clone();
        let status = state.status.clone();

        // Cleaned-up output: no bytes, just the terminal status and a note.
        if !state.disposition.rings_present() {
            let note = match &state.disposition {
                OutputDisposition::Expired { .. } => {
                    "output expired: evicted to reclaim the session output quota".to_owned()
                }
                _ => "output fully consumed; nothing more to read".to_owned(),
            };
            return Ok(Some(TaskRead {
                status,
                stdout: String::new(),
                stderr: String::new(),
                stdout_lost: 0,
                stderr_lost: 0,
                note: Some(note),
            }));
        }

        let out = record
            .files()
            .stdout
            .read_from(state.stdout_cursor, READ_CHUNK_LIMIT)
            .await
            .map_err(TaskAccessError::from)?;
        let err = record
            .files()
            .stderr
            .read_from(state.stderr_cursor, READ_CHUNK_LIMIT)
            .await
            .map_err(TaskAccessError::from)?;
        let terminal = !status.is_running();
        let (stdout, out_carry) = decode_with_carry(
            &state.stdout_carry,
            &out.bytes,
            out.lost,
            terminal && !out.has_more,
        );
        let (stderr, err_carry) = decode_with_carry(
            &state.stderr_carry,
            &err.bytes,
            err.lost,
            terminal && !err.has_more,
        );

        // Persist the advanced cursors + carry before returning any bytes.
        let mut candidate = state;
        candidate.stdout_cursor = out.next_cursor;
        candidate.stderr_cursor = err.next_cursor;
        candidate.stdout_carry = out_carry;
        candidate.stderr_carry = err_carry;
        guard
            .commit(candidate)
            .await
            .map_err(TaskAccessError::from)?;
        drop(guard);

        // If this read drained a terminal task, reclaim its output.
        if terminal && !out.has_more && !err.has_more {
            let _ = backend.quota.finalize_consumed(&record).await;
        }

        Ok(Some(TaskRead {
            status,
            stdout,
            stderr,
            stdout_lost: out.lost,
            stderr_lost: err.lost,
            note: None,
        }))
    }

    /// Request termination and wait for the monitor's *full* commit — the
    /// published terminal summary, not just the status flip, so an immediate
    /// `take_notices` after returning sees the completion. Idempotent; returns
    /// the settled status, `Ok(None)` for an unknown id.
    pub async fn kill(&self, id: &TaskId) -> Result<Option<TaskStatus>, TaskAccessError> {
        let backend = self.enabled()?;
        let live = self.inner.lock().await.tasks.get(id).cloned();
        let Some(entry) = live else {
            // Not live: report the archived task's terminal status, if any.
            return match backend
                .archive
                .open(id)
                .await
                .map_err(TaskAccessError::from)?
            {
                Some(record) => Ok(Some(record.lock_commit().await.current().status.clone())),
                None => Ok(None),
            };
        };

        let mut rx = self.summaries_rx.clone();
        entry.cancel.cancel();
        loop {
            {
                let summaries = rx.borrow_and_update();
                match summaries
                    .iter()
                    .find(|summary| summary.id == entry.id().as_str())
                {
                    // Terminal in the published snapshot: the commit (notice
                    // included — publish is its last step) is complete.
                    Some(summary) if !summary.status.is_running() => {
                        return Ok(Some(summary.status.clone()));
                    }
                    Some(_) => {}
                    // Absent: reclaimed, which only happens post-commit.
                    None => break,
                }
            }
            if rx.changed().await.is_err() {
                break; // registry gone; fall back to the record's state
            }
        }
        Ok(Some(
            entry.record.lock_commit().await.current().status.clone(),
        ))
    }

    fn enabled(&self) -> Result<Arc<Backend>, TaskAccessError> {
        match &self.store {
            Store::Enabled(b) => Ok(b.clone()),
            Store::Disabled(e) => Err(TaskAccessError::Disabled(e.to_string())),
        }
    }

    /// Drain accumulated notices (the overflow aggregate last).
    pub async fn take_notices(&self) -> Vec<TaskNotice> {
        let mut inner = self.inner.lock().await;
        drain_notices(&mut inner)
    }

    /// Re-enqueue notices persisted by a previous incarnation: full notices
    /// (`Task`/`OutputExpired`) go ahead of any accumulated since, and a
    /// restored aggregate merges into the overflow slot keeping its batch id.
    /// Once per registry instance (the caller guarantees once per hub entry).
    pub async fn restore_notices(&self, restored: Vec<TaskNotice>) {
        if restored.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().await;
        let mut fulls = Vec::new();
        for notice in restored {
            match notice {
                TaskNotice::Task { .. } | TaskNotice::OutputExpired { .. } => fulls.push(notice),
                TaskNotice::Overflow {
                    batch_id,
                    dropped,
                    uncounted,
                } => {
                    inner.merge_overflow_batch(batch_id, dropped, uncounted);
                }
            }
        }
        let newer = std::mem::replace(&mut inner.notices, fulls);
        inner.notices.extend(newer);
    }

    /// Live overview of every retained task. Watch semantics: subscribing
    /// yields the current value immediately; every terminal commit and spawn
    /// publishes. Keepalive watchers count `Running` entries here.
    pub fn summaries(&self) -> watch::Receiver<Arc<[TaskSummary]>> {
        self.summaries_rx.clone()
    }

    /// Close the registry (further spawns fail), kill everything still
    /// running, wait for every monitor to finish committing, and return all
    /// undelivered notices. Idempotent; concurrent callers serialize, so
    /// none returns before the teardown barrier holds.
    pub async fn shutdown(&self) -> Vec<TaskNotice> {
        let _gate = self.shutdown_gate.lock().await;
        let (entries, monitors) = {
            let mut inner = self.inner.lock().await;
            inner.closed = true;
            let entries: Vec<Arc<TaskEntry>> = inner.tasks.values().cloned().collect();
            let monitors: Vec<JoinHandle<()>> = inner.monitors.drain().map(|(_, h)| h).collect();
            (entries, monitors)
        };
        for entry in &entries {
            entry.cancel.cancel();
        }
        // Join monitors *before* draining: every terminal state and notice is
        // committed (rings flushed, manifests saved) by the time we collect them.
        for monitor in monitors {
            let _ = monitor.await;
        }
        let mut inner = self.inner.lock().await;
        let notices = drain_notices(&mut inner);
        // Wake watchers even when nothing changed (e.g. zero tasks) so a
        // keepalive watcher parked on this registry re-checks its entry and
        // can retire once the entry is released.
        inner.publish();
        notices
    }
}

/// Drain the notices and the overflow aggregate (aggregate last).
fn drain_notices(inner: &mut RegistryState) -> Vec<TaskNotice> {
    let mut notices = std::mem::take(&mut inner.notices);
    if let Some(slot) = inner.overflow.take() {
        notices.push(TaskNotice::Overflow {
            batch_id: slot.batch_id,
            dropped: slot.dropped,
            uncounted: slot.uncounted,
        });
    }
    notices
}

/// Errors from `read`/`kill` distinct from an unknown id (`Ok(None)`).
#[derive(Debug)]
pub enum TaskAccessError {
    /// Background storage is disabled (the archive root could not be opened).
    Disabled(String),
    /// The archive entry is present but corrupt, or an I/O error occurred.
    Archive(String),
}

impl std::fmt::Display for TaskAccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskAccessError::Disabled(e) => write!(f, "background tasks are disabled: {e}"),
            TaskAccessError::Archive(e) => write!(f, "background task archive error: {e}"),
        }
    }
}

impl std::error::Error for TaskAccessError {}

impl From<ArchiveError> for TaskAccessError {
    fn from(e: ArchiveError) -> Self {
        TaskAccessError::Archive(e.to_string())
    }
}

impl From<std::io::Error> for TaskAccessError {
    fn from(e: std::io::Error) -> Self {
        TaskAccessError::Archive(e.to_string())
    }
}

/// Decode a chunk of raw output into a `String`, carrying a trailing incomplete
/// UTF-8 sequence to the next read. The byte cursor never regresses: carried
/// bytes are stored (not re-read), and a chunk boundary that split a scalar is
/// stitched with `prev_carry`. When `flush` (terminal EOF with no more bytes),
/// any trailing incomplete bytes are emitted as U+FFFD rather than carried. A
/// consumer loss (`lost > 0`) discards a now-orphaned carry as one U+FFFD.
fn decode_with_carry(prev_carry: &[u8], bytes: &[u8], lost: u64, flush: bool) -> (String, Vec<u8>) {
    let mut out = String::new();
    let mut work: Vec<u8> = Vec::with_capacity(prev_carry.len() + bytes.len());
    if lost > 0 && !prev_carry.is_empty() {
        // The carry's continuation was overwritten before we could read it.
        out.push('\u{FFFD}');
    } else {
        work.extend_from_slice(prev_carry);
    }
    work.extend_from_slice(bytes);

    match std::str::from_utf8(&work) {
        Ok(s) => {
            out.push_str(s);
            (out, Vec::new())
        }
        Err(e) => {
            let valid = e.valid_up_to();
            // SAFETY-free: valid..end is guaranteed valid UTF-8.
            out.push_str(std::str::from_utf8(&work[..valid]).unwrap());
            let rest = &work[valid..];
            match e.error_len() {
                // Trailing incomplete sequence: carry it unless we must flush.
                None if !flush && rest.len() <= 3 => (out, rest.to_vec()),
                _ => {
                    out.push_str(&String::from_utf8_lossy(rest));
                    (out, Vec::new())
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum StreamName {
    Stdout,
    Stderr,
}

impl StreamName {
    fn label(self) -> &'static str {
        match self {
            StreamName::Stdout => "stdout",
            StreamName::Stderr => "stderr",
        }
    }
}

/// How a pipe pump ended: clean EOF, or an irrecoverable read/spool failure
/// (message already includes the stream and the cause).
enum PumpResult {
    Eof,
    Failed { message: String },
}

/// Drives one background process: pumps stdout/stderr into the ring files and
/// resolves when the leader exits and the pipes drain. A pump read/spool
/// failure is terminal — the group is killed and the task settles `Failed`,
/// never a clean exit. Cancellation (kill/shutdown) SIGKILLs the group and is
/// biased to win over a concurrent failure so a user kill stays `Killed`. Pipe
/// drains after a group kill are bounded, so a setsid descendant holding a pipe
/// cannot stall the terminal commit.
async fn run_process(mut group: GroupedChild, ctx: TaskCtx) -> TaskExit {
    let stdout = group.child.stdout.take().expect("stdout is piped");
    let stderr = group.child.stderr.take().expect("stderr is piped");
    let mut out_pump = tokio::spawn(pump_stream(stdout, ctx.clone(), StreamName::Stdout));
    let mut err_pump = tokio::spawn(pump_stream(stderr, ctx.clone(), StreamName::Stderr));
    let cancel = ctx.cancelled();

    let mut out_res: Option<PumpResult> = None;
    let mut err_res: Option<PumpResult> = None;
    let mut exited: Option<Option<i32>> = None;
    let mut failure: Option<String> = None;

    loop {
        // Cancellation wins over everything (biased below reinforces this).
        if cancel.is_cancelled() {
            group.kill_group();
            drain_pump(&mut out_pump, &mut out_res).await;
            drain_pump(&mut err_pump, &mut err_res).await;
            let _ = group.child.wait().await;
            return TaskExit::Killed;
        }
        // A spool/read failure is terminal even if the leader already exited.
        if let Some(message) = failure {
            group.kill_group();
            drain_pump(&mut out_pump, &mut out_res).await;
            drain_pump(&mut err_pump, &mut err_res).await;
            let _ = group.child.wait().await;
            return TaskExit::Failed { message };
        }
        // Natural completion: leader reaped and both pipes drained to EOF.
        if let Some(code) = exited
            && out_res.is_some()
            && err_res.is_some()
        {
            group.disarm();
            return TaskExit::Exited { code };
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {}
            res = &mut out_pump, if out_res.is_none() => {
                record_pump(res, &mut out_res, &mut failure);
            }
            res = &mut err_pump, if err_res.is_none() => {
                record_pump(res, &mut err_res, &mut failure);
            }
            status = group.child.wait(), if exited.is_none() => {
                exited = Some(status.ok().and_then(|s| s.code()));
            }
        }
    }
}

/// Store a completed pump's result, recording the first failure cause.
fn record_pump(
    res: Result<PumpResult, tokio::task::JoinError>,
    slot: &mut Option<PumpResult>,
    failure: &mut Option<String>,
) {
    let result = res.unwrap_or(PumpResult::Eof);
    if let PumpResult::Failed { message } = &result
        && failure.is_none()
    {
        *failure = Some(message.clone());
    }
    *slot = Some(result);
}

/// One stream's pump loop: read the pipe, append to the ring. A read or append
/// error ends the pump with a structured failure rather than a silent EOF.
async fn pump_stream<R>(mut reader: R, ctx: TaskCtx, stream: StreamName) -> PumpResult
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => return PumpResult::Eof,
            Ok(n) => {
                let appended = match stream {
                    StreamName::Stdout => ctx.append_stdout(&buf[..n]).await,
                    StreamName::Stderr => ctx.append_stderr(&buf[..n]).await,
                };
                if let Err(e) = appended {
                    return PumpResult::Failed {
                        message: format!("{} spool write failed: {e}", stream.label()),
                    };
                }
            }
            Err(e) => {
                return PumpResult::Failed {
                    message: format!("{} read failed: {e}", stream.label()),
                };
            }
        }
    }
}

/// Bounded wait for a pipe pump after a group kill; on expiry it is aborted and
/// its result treated as EOF, so an escaped descendant cannot stall teardown.
async fn drain_pump(pump: &mut JoinHandle<PumpResult>, slot: &mut Option<PumpResult>) {
    if slot.is_some() {
        return;
    }
    match tokio::time::timeout(PIPE_DRAIN_TIMEOUT, &mut *pump).await {
        Ok(res) => *slot = Some(res.unwrap_or(PumpResult::Eof)),
        Err(_) => {
            pump.abort();
            *slot = Some(PumpResult::Eof);
        }
    }
}

/// Snapshot for a task's terminal notice/summary, captured before any cleanup
/// deletes the ring files.
struct TerminalOutcome {
    status: TaskStatus,
    tail: String,
    stdout_overwritten: u64,
    stderr_overwritten: u64,
}

/// Awaits the task's work and commits the terminal state — the single writer of
/// that transition. Order (load-bearing): terminal manifest commit first, then
/// quota finalize (Consumed cleanup or victim registration), then (under the
/// registry lock) notice enqueue, bookkeeping, and the summaries publish *last*
/// so a watcher seeing zero running already has the notice drainable.
async fn monitor_task(
    inner: Arc<Mutex<RegistryState>>,
    backend: Arc<Backend>,
    entry: Arc<TaskEntry>,
    work: impl Future<Output = TaskExit>,
) {
    let exit = work.await;
    let record = &entry.record;
    let outcome = commit_terminal(record, exit).await;

    // Reclaim (Consumed) or register as an eviction victim. Cleanup failure is
    // only logged: the completion notice and summary must still publish.
    if let Err(e) = backend.quota.finalize_terminal(record).await {
        tracing::warn!(task = record.id().as_str(), error = %e, "task output finalize failed");
    }

    let id = record.id().clone();
    let mut inner = inner.lock().await;
    inner.push_notice(TaskNotice::Task {
        id: id.clone(),
        command: record.meta().command.clone(),
        description: record.meta().description.clone(),
        status: outcome.status.clone(),
        output_tail: outcome.tail,
        stdout_overwritten: outcome.stdout_overwritten,
        stderr_overwritten: outcome.stderr_overwritten,
    });
    inner.running_count -= 1;
    inner.terminal_order.push_back(id.clone());
    if inner.terminal_order.len() > MAX_TERMINAL
        && let Some(oldest) = inner.terminal_order.pop_front()
    {
        inner.tasks.remove(&oldest);
        inner.summaries.remove(&oldest);
        // A reclaimed task's monitor has long finished; drop its handle.
        inner.monitors.remove(&oldest);
    }
    if let Some(summary) = inner.summaries.get_mut(&id) {
        summary.status = outcome.status;
    }
    inner.publish();
}

/// Flush the rings, snapshot the notice tail/overwrite totals, and atomically
/// commit the terminal manifest. A flush or save failure degrades the task to
/// `Failed` rather than reporting a clean exit whose output was not persisted.
async fn commit_terminal(record: &TaskRecord, exit: TaskExit) -> TerminalOutcome {
    let at = jiff::Timestamp::now();
    let intended = match exit {
        TaskExit::Exited { code } => TaskStatus::Exited { code, at },
        TaskExit::Killed => TaskStatus::Killed { at },
        TaskExit::Failed { message } => TaskStatus::Failed { message, at },
    };
    // Flush so the terminal manifest's logical range is durable, then snapshot
    // the overwrite totals and tail before any cleanup can delete the rings.
    let flush_ok = record.files().flush().await.is_ok();
    let stdout_overwritten = record.files().stdout.logical_range().await.0;
    let stderr_overwritten = record.files().stderr.logical_range().await.0;
    let tail = terminal_tail(record).await;

    let mut guard = record.lock_commit().await;
    if !guard.current().status.is_running() {
        // Already terminal (e.g. a concurrent kill settled first).
        return TerminalOutcome {
            status: guard.current().status.clone(),
            tail,
            stdout_overwritten,
            stderr_overwritten,
        };
    }
    let mut candidate = guard.current().clone();
    candidate.status = intended.clone();
    let committed = flush_ok && guard.commit(candidate).await.is_ok();
    let status = if committed {
        intended
    } else {
        let failed = TaskStatus::Failed {
            message: "background output spool save failed".into(),
            at,
        };
        let mut degraded = guard.current().clone();
        degraded.status = failed.clone();
        let _ = guard.commit(degraded).await; // best-effort
        failed
    };
    TerminalOutcome {
        status,
        tail,
        stdout_overwritten,
        stderr_overwritten,
    }
}

/// The notice tail: the last bytes of stdout, or stderr if stdout is empty.
async fn terminal_tail(record: &TaskRecord) -> String {
    let out = record
        .files()
        .stdout
        .tail(NOTICE_TAIL_LIMIT)
        .await
        .unwrap_or_default();
    if !out.is_empty() {
        return String::from_utf8_lossy(&out).into_owned();
    }
    let err = record
        .files()
        .stderr
        .tail(NOTICE_TAIL_LIMIT)
        .await
        .unwrap_or_default();
    String::from_utf8_lossy(&err).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::Notify;

    /// The ring capacity a fresh stream is created with — the retained window.
    const TAIL_BUF_CAP: usize = DEFAULT_STREAM_CAPACITY as usize;

    fn meta(command: &str) -> TaskMeta {
        TaskMeta {
            command: command.into(),
            description: "test task".into(),
            agent_name: "coda".into(),
        }
    }

    fn running_count(rx: &watch::Receiver<Arc<[TaskSummary]>>) -> usize {
        rx.borrow().iter().filter(|s| s.status.is_running()).count()
    }

    /// spawn publishes the running task before returning the id.
    #[tokio::test]
    async fn spawn_publishes_keepalive_before_returning() {
        let reg = BackgroundProcesses::new();
        let rx = reg.summaries();
        assert_eq!(running_count(&rx), 0);
        let gate = Arc::new(Notify::new());
        let g = gate.clone();
        let id = reg
            .spawn_with(meta("sleep"), |_ctx| async move {
                g.notified().await;
                TaskExit::Exited { code: Some(0) }
            })
            .await
            .unwrap();
        // No await between spawn returning and this check: the watch already
        // carries the running task.
        assert_eq!(running_count(&rx), 1);
        assert!(rx.borrow().iter().any(|s| s.id == id.as_str()));
        gate.notify_one();
    }

    /// When a watcher observes zero running tasks, the notice is already
    /// drainable (publish is the last step of the commit).
    #[tokio::test]
    async fn notice_is_enqueued_before_zero_is_visible() {
        let reg = BackgroundProcesses::new();
        let mut rx = reg.summaries();
        let gate = Arc::new(Notify::new());
        let g = gate.clone();
        reg.spawn_with(meta("quick"), |ctx| async move {
            ctx.append_stdout(b"done!").await.unwrap();
            g.notified().await;
            TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
        gate.notify_one();
        loop {
            rx.changed().await.unwrap();
            if running_count(&rx) == 0 {
                break;
            }
        }
        let notices = reg.take_notices().await;
        assert_eq!(notices.len(), 1);
        assert!(matches!(
            &notices[0],
            TaskNotice::Task { status: TaskStatus::Exited { code: Some(0), .. }, output_tail, .. }
                if output_tail == "done!"
        ));
    }

    /// kill vs natural exit: exactly one terminal state, one notice.
    #[tokio::test]
    async fn kill_racing_natural_exit_settles_once() {
        let reg = BackgroundProcesses::new();
        let id = reg
            .spawn_with(meta("racy"), |ctx| async move {
                let cancel = ctx.cancelled();
                tokio::select! {
                    _ = cancel.cancelled() => TaskExit::Killed,
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {
                        TaskExit::Exited { code: Some(0) }
                    }
                }
            })
            .await
            .unwrap();
        // Race the kill against the natural exit; either way the commit is
        // singular.
        tokio::time::sleep(Duration::from_millis(1)).await;
        let status = reg.kill(&id).await.unwrap().expect("task known");
        assert!(!status.is_running());
        let notices = reg.take_notices().await;
        assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
        assert!(reg.take_notices().await.is_empty());
    }

    /// shutdown joins monitors before draining, so killed tasks' notices are
    /// in the returned batch; afterwards spawn is rejected.
    #[tokio::test]
    async fn shutdown_returns_notices_of_killed_tasks_and_closes() {
        let reg = BackgroundProcesses::new();
        let id = reg
            .spawn_with(meta("forever"), |ctx| async move {
                ctx.cancelled().cancelled().await;
                TaskExit::Killed
            })
            .await
            .unwrap();
        let notices = reg.shutdown().await;
        assert!(
            notices.iter().any(|n| matches!(
                n,
                TaskNotice::Task { id: nid, status: TaskStatus::Killed { .. }, .. } if *nid == id
            )),
            "killed task must be in the shutdown batch: {notices:?}"
        );
        let err = reg
            .spawn_with(meta("late"), |_ctx| async { TaskExit::Killed })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("shut down"));
        // Idempotent.
        assert!(reg.shutdown().await.is_empty());
    }

    #[tokio::test]
    async fn running_limit_rejects_spawn() {
        let reg = BackgroundProcesses::new();
        let gate = Arc::new(Notify::new());
        for _ in 0..MAX_RUNNING {
            let g = gate.clone();
            reg.spawn_with(meta("filler"), move |ctx| async move {
                let cancel = ctx.cancelled();
                tokio::select! {
                    _ = g.notified() => TaskExit::Exited { code: Some(0) },
                    // Cancel-aware, or the shutdown below joins forever.
                    _ = cancel.cancelled() => TaskExit::Killed,
                }
            })
            .await
            .unwrap();
        }
        let err = reg
            .spawn_with(meta("overflow"), |_ctx| async { TaskExit::Killed })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too many"));
        reg.shutdown().await;
    }

    /// Incremental reads move an absolute cursor; a truncated head is
    /// reported as lost bytes, never re-read or skipped.
    #[tokio::test]
    async fn read_reports_lost_bytes_after_truncation() {
        let reg = BackgroundProcesses::new();
        let gate = Arc::new(Notify::new());
        let g = gate.clone();
        let id = reg
            .spawn_with(meta("chatty"), move |ctx| async move {
                ctx.append_stdout(b"first").await.unwrap();
                g.notified().await;
                // Blow past the buffer cap so the head (including anything
                // unread) is dropped.
                let big = vec![b'x'; TAIL_BUF_CAP + 7];
                ctx.append_stdout(&big).await.unwrap();
                ctx.cancelled().cancelled().await;
                TaskExit::Killed
            })
            .await
            .unwrap();

        // First read consumes "first" (5 bytes, cursor -> 5).
        let mut seen = String::new();
        while seen.len() < 5 {
            let read = reg.read(&id).await.unwrap().unwrap();
            seen.push_str(&read.stdout);
            assert_eq!(read.stdout_lost, 0);
            tokio::task::yield_now().await;
        }
        assert_eq!(seen, "first");
        gate.notify_one();

        // Wait until the big write landed, then drain: reads are chunked, so
        // the retained window (cap bytes) comes back across several calls. Loss
        // is reported exactly once, on the first read after the truncation.
        let mut lost_total = 0u64;
        let mut drained = 0usize;
        loop {
            let read = reg.read(&id).await.unwrap().unwrap();
            lost_total += read.stdout_lost;
            drained += read.stdout.len();
            if drained >= TAIL_BUF_CAP {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            lost_total, 7,
            "bytes dropped before the read are reported once"
        );
        assert_eq!(
            drained, TAIL_BUF_CAP,
            "the whole retained window is drained"
        );
        // Cursor is at total_written now: nothing further, nothing repeated.
        let read = reg.read(&id).await.unwrap().unwrap();
        assert_eq!(read.stdout.len(), 0);
        assert_eq!(read.stdout_lost, 0);
        reg.shutdown().await;
    }

    /// Terminal entries beyond MAX_TERMINAL are reclaimed from the in-memory
    /// live overview oldest-first, but stay readable by id from the archive:
    /// memory reclamation is decoupled from disk retention.
    #[tokio::test]
    async fn terminal_entries_are_reclaimed_beyond_cap() {
        let reg = BackgroundProcesses::new();
        let mut first_id = None;
        for i in 0..(MAX_TERMINAL + 1) {
            let id = reg
                .spawn_with(meta(&format!("t{i}")), |_ctx| async {
                    TaskExit::Exited { code: Some(0) }
                })
                .await
                .unwrap();
            first_id.get_or_insert(id.clone());
            // Settle each task before spawning the next so terminal order is
            // deterministic.
            let mut rx = reg.summaries();
            loop {
                let done = rx
                    .borrow_and_update()
                    .iter()
                    .any(|s| s.id == id.as_str() && !s.status.is_running());
                let gone = i > 0 && !rx.borrow().iter().any(|s| s.id == id.as_str());
                if done || gone {
                    break;
                }
                rx.changed().await.unwrap();
            }
        }
        let first = first_id.unwrap();
        // Reclaimed from the live overview...
        assert!(
            !reg.summaries()
                .borrow()
                .iter()
                .any(|s| s.id == first.as_str()),
            "oldest terminal task is reclaimed from the live overview"
        );
        // ...but still readable by id from the archive on disk.
        assert!(
            reg.read(&first).await.unwrap().is_some(),
            "a memory-reclaimed terminal task stays readable from disk"
        );
        // Its notice still exists — reclamation frees the overview slot, not facts.
        let notices = reg.take_notices().await;
        assert_eq!(notices.len(), MAX_TERMINAL + 1);
        reg.shutdown().await;
    }

    /// Full notices beyond the cap degrade into the overflow aggregate; the
    /// aggregate itself is never dropped.
    #[tokio::test]
    async fn notice_overflow_degrades_into_aggregate() {
        let reg = BackgroundProcesses::new();
        for i in 0..(MAX_FULL_NOTICES + 3) {
            let id = reg
                .spawn_with(meta(&format!("n{i}")), |_ctx| async {
                    TaskExit::Exited { code: Some(0) }
                })
                .await
                .unwrap();
            let mut rx = reg.summaries();
            loop {
                let settled = rx
                    .borrow_and_update()
                    .iter()
                    .any(|s| s.id == id.as_str() && !s.status.is_running())
                    || !rx.borrow().iter().any(|s| s.id == id.as_str());
                if settled {
                    break;
                }
                rx.changed().await.unwrap();
            }
        }
        let notices = reg.take_notices().await;
        let full = notices
            .iter()
            .filter(|n| matches!(n, TaskNotice::Task { .. }))
            .count();
        assert_eq!(full, MAX_FULL_NOTICES);
        let overflow: Vec<_> = notices
            .iter()
            .filter_map(|n| match n {
                TaskNotice::Overflow {
                    dropped, uncounted, ..
                } => Some((dropped.len(), *uncounted)),
                _ => None,
            })
            .collect();
        assert_eq!(overflow, vec![(3, 0)]);
        reg.shutdown().await;
    }

    fn completed_notice(id: &TaskId) -> TaskNotice {
        TaskNotice::Task {
            id: id.clone(),
            command: "old".into(),
            description: String::new(),
            status: TaskStatus::Killed {
                at: jiff::Timestamp::now(),
            },
            output_tail: String::new(),
            stdout_overwritten: 0,
            stderr_overwritten: 0,
        }
    }

    fn fact_ids(dropped: &[TaskNoticeFact]) -> Vec<String> {
        dropped
            .iter()
            .map(|f| match f {
                TaskNoticeFact::Completed { id, .. } => id.as_str().to_owned(),
                TaskNoticeFact::OutputExpired { id, .. } => id.as_str().to_owned(),
            })
            .collect()
    }

    /// A restored aggregate merges into the overflow slot — never into the
    /// full-notice queue, where the demotion path only expects full notices.
    #[tokio::test]
    async fn restore_merges_persisted_aggregate_into_overflow_slot() {
        let reg = BackgroundProcesses::new();
        let old = TaskId::new();
        let x = TaskId::new();
        reg.restore_notices(vec![
            completed_notice(&old),
            TaskNotice::Overflow {
                batch_id: "batch-1".into(),
                dropped: vec![TaskNoticeFact::Completed {
                    id: x.clone(),
                    status: TaskStatus::Killed {
                        at: jiff::Timestamp::now(),
                    },
                }],
                uncounted: 5,
            },
        ])
        .await;

        // Drive exactly enough completions to force one demotion: the
        // restored full notice is the oldest and degrades into the slot.
        for i in 0..MAX_FULL_NOTICES {
            let id = reg
                .spawn_with(meta(&format!("r{i}")), |_ctx| async {
                    TaskExit::Exited { code: Some(0) }
                })
                .await
                .unwrap();
            let mut rx = reg.summaries();
            loop {
                let settled = rx
                    .borrow_and_update()
                    .iter()
                    .any(|s| s.id == id.as_str() && !s.status.is_running())
                    || !rx.borrow().iter().any(|s| s.id == id.as_str());
                if settled {
                    break;
                }
                rx.changed().await.unwrap();
            }
        }

        let notices = reg.take_notices().await;
        let fulls = notices
            .iter()
            .filter(|n| matches!(n, TaskNotice::Task { .. }))
            .count();
        assert_eq!(fulls, MAX_FULL_NOTICES);
        let (dropped, uncounted) = notices
            .iter()
            .find_map(|n| match n {
                TaskNotice::Overflow {
                    dropped, uncounted, ..
                } => Some((dropped, *uncounted)),
                _ => None,
            })
            .expect("aggregate present");
        assert_eq!(
            fact_ids(dropped),
            vec![x.as_str().to_owned(), old.as_str().to_owned()]
        );
        assert_eq!(uncounted, 5);
        reg.shutdown().await;
    }

    /// Concurrent shutdowns serialize on the teardown barrier: the notice of
    /// the killed task lands in exactly one drain, and neither call returns
    /// with work still running.
    #[tokio::test]
    async fn concurrent_shutdowns_share_the_barrier() {
        let reg = Arc::new(BackgroundProcesses::new());
        reg.spawn_with(meta("forever"), |ctx| async move {
            ctx.cancelled().cancelled().await;
            TaskExit::Killed
        })
        .await
        .unwrap();

        let (a, b) = tokio::join!(reg.shutdown(), reg.shutdown());
        assert_eq!(
            a.len() + b.len(),
            1,
            "one notice, in exactly one drain: {a:?} / {b:?}"
        );
        let rx = reg.summaries();
        assert_eq!(running_count(&rx), 0);
    }

    /// Restored notices come out ahead of ones accumulated since.
    #[tokio::test]
    async fn restore_notices_orders_before_new_completions() {
        let reg = BackgroundProcesses::new();
        let id = reg
            .spawn_with(meta("new"), |_ctx| async {
                TaskExit::Exited { code: Some(0) }
            })
            .await
            .unwrap();
        let mut rx = reg.summaries();
        while running_count(&rx) > 0 {
            rx.changed().await.unwrap();
        }
        let old = TaskId::new();
        reg.restore_notices(vec![completed_notice(&old)]).await;
        let notices = reg.take_notices().await;
        // Restored notice first, then the completion accumulated since.
        let ids: Vec<Vec<TaskNoticeKey>> = notices.iter().map(|n| n.keys()).collect();
        assert_eq!(
            ids,
            vec![
                vec![TaskNoticeKey::Completed {
                    task_id: old.as_str().to_owned()
                }],
                vec![TaskNoticeKey::Completed {
                    task_id: id.as_str().to_owned()
                }],
            ]
        );
    }

    // ---- real-process tasks -------------------------------------------

    fn bash(command: &str) -> Command {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(command);
        cmd
    }

    fn process_alive(pid: i32) -> bool {
        // SAFETY: signal 0 only probes for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Kills a helper process this test spawned, even if an assertion fails.
    struct KillPidGuard(i32);

    impl Drop for KillPidGuard {
        fn drop(&mut self) {
            // SAFETY: plain signal syscall on the helper this test spawned.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }

    async fn wait_pids(pidfile: &std::path::Path, expect: usize) -> Vec<i32> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(content) = std::fs::read_to_string(pidfile) {
                    let pids: Vec<i32> = content
                        .split_whitespace()
                        .filter_map(|p| p.parse().ok())
                        .collect();
                    if pids.len() == expect {
                        break pids;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("command never wrote its pidfile")
    }

    async fn assert_pids_die(pids: &[i32]) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if pids.iter().all(|&pid| !process_alive(pid)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            let survivors: Vec<i32> = pids
                .iter()
                .copied()
                .filter(|&pid| process_alive(pid))
                .collect();
            for &pid in &survivors {
                // SAFETY: plain signal syscall on processes this test spawned.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            panic!("processes survived the group kill: {survivors:?}");
        });
    }

    /// spawn → incremental reads observe streamed output → natural exit
    /// commits the code and produces a notice carrying the tail.
    #[tokio::test]
    async fn process_task_streams_output_and_notifies_on_exit() {
        let reg = BackgroundProcesses::new();
        let id = reg
            .spawn(
                bash("echo out-marker; echo err-marker >&2; exit 3"),
                meta("markers"),
            )
            .await
            .unwrap();

        let (mut stdout, mut stderr) = (String::new(), String::new());
        let status = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let read = reg.read(&id).await.unwrap().expect("task known");
                stdout.push_str(&read.stdout);
                stderr.push_str(&read.stderr);
                if !read.status.is_running() {
                    break read.status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("process never settled");
        // One more read: the terminal commit happens after the pumps flushed,
        // so the remainder is fully readable.
        let last = reg.read(&id).await.unwrap().unwrap();
        stdout.push_str(&last.stdout);
        stderr.push_str(&last.stderr);

        assert_eq!(
            status, last.status,
            "terminal status is stable across reads"
        );
        assert!(matches!(status, TaskStatus::Exited { code: Some(3), .. }));
        assert_eq!(stdout, "out-marker\n");
        assert_eq!(stderr, "err-marker\n");

        let notices = reg.take_notices().await;
        assert_eq!(notices.len(), 1);
        assert!(matches!(
            &notices[0],
            TaskNotice::Task { id: nid, status: TaskStatus::Exited { code: Some(3), .. }, output_tail, .. }
                if *nid == id && output_tail.contains("out-marker")
        ));
        reg.shutdown().await;
    }

    /// kill terminates the whole process group — bash and its forked child —
    /// and returns only after the full commit (notice drainable).
    #[tokio::test]
    async fn kill_kills_the_whole_process_group() {
        let pidfile = std::env::temp_dir().join(format!("coda-bg-group-{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);

        let reg = BackgroundProcesses::new();
        let command = format!(
            "sleep 38.21 & echo \"$$ $!\" > '{}'; wait",
            pidfile.display()
        );
        let id = reg.spawn(bash(&command), meta("group")).await.unwrap();
        let pids = wait_pids(&pidfile, 2).await;

        let status = reg.kill(&id).await.unwrap().expect("task known");
        assert!(matches!(status, TaskStatus::Killed { .. }));
        assert_pids_die(&pids).await;

        let notices = reg.take_notices().await;
        assert!(
            notices.iter().any(|n| matches!(
                n,
                TaskNotice::Task { id: nid, status: TaskStatus::Killed { .. }, .. } if *nid == id
            )),
            "kill's notice must be drainable once kill returned: {notices:?}"
        );
        let _ = std::fs::remove_file(&pidfile);
        reg.shutdown().await;
    }

    /// A shell leader may exit before a background child closes its inherited
    /// pipes. Killing while the runner drains those pipes is still a kill,
    /// not the leader's earlier successful exit.
    #[tokio::test]
    async fn kill_after_leader_exit_reports_killed() {
        let pidfile =
            std::env::temp_dir().join(format!("coda-bg-exited-leader-{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);

        let reg = BackgroundProcesses::new();
        let command = format!(
            "sleep 38.31 & echo \"$$ $!\" > '{}'; exit 0",
            pidfile.display()
        );
        let id = reg
            .spawn(bash(&command), meta("exited leader"))
            .await
            .unwrap();
        let pids = wait_pids(&pidfile, 2).await;
        let _child_cleanup = KillPidGuard(pids[1]);

        tokio::time::timeout(Duration::from_secs(5), async {
            while process_alive(pids[0]) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("shell leader did not exit");
        assert!(
            reg.read(&id)
                .await
                .unwrap()
                .expect("task known")
                .status
                .is_running(),
            "background child should keep the task draining"
        );

        let status = reg.kill(&id).await.unwrap().expect("task known");
        assert!(matches!(status, TaskStatus::Killed { .. }));
        assert_pids_die(&pids[1..]).await;

        let _ = std::fs::remove_file(&pidfile);
        reg.shutdown().await;
    }

    /// A setsid descendant escapes the group kill while holding the stdout
    /// pipe open; the bounded drain still lets kill commit promptly.
    #[tokio::test]
    async fn kill_settles_promptly_when_a_descendant_escapes_the_group() {
        let ready = std::env::temp_dir().join(format!("coda-bg-escape-{}", std::process::id()));
        let _ = std::fs::remove_file(&ready);

        let reg = BackgroundProcesses::new();
        let command = format!(
            "perl -MPOSIX -e 'POSIX::setsid(); open my $f, \">\", $ARGV[0]; print $f $$; close $f; exec \"sleep\", \"38.41\"' '{}' & wait",
            ready.display()
        );
        let id = reg.spawn(bash(&command), meta("escape")).await.unwrap();
        let escapee = wait_pids(&ready, 1).await[0];
        let _cleanup = KillPidGuard(escapee);

        // Must settle within the bounded drain, not when the sleep exits.
        let status = tokio::time::timeout(Duration::from_secs(2), reg.kill(&id))
            .await
            .expect("kill hung on the escaped descendant's pipe")
            .unwrap()
            .expect("task known");
        assert!(matches!(status, TaskStatus::Killed { .. }));

        let _ = std::fs::remove_file(&ready);
        reg.shutdown().await;
    }

    /// shutdown kills every running process group and leaves no residue.
    #[tokio::test]
    async fn shutdown_leaves_no_process_residue() {
        let pidfile = std::env::temp_dir().join(format!("coda-bg-shutdown-{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);

        let reg = BackgroundProcesses::new();
        let command = format!(
            "sleep 38.61 & echo \"$$ $!\" > '{}'; wait",
            pidfile.display()
        );
        let id = reg.spawn(bash(&command), meta("residue")).await.unwrap();
        let pids = wait_pids(&pidfile, 2).await;

        let notices = reg.shutdown().await;
        assert!(
            notices.iter().any(|n| matches!(
                n,
                TaskNotice::Task { id: nid, status: TaskStatus::Killed { .. }, .. } if *nid == id
            )),
            "shutdown returns the killed task's notice: {notices:?}"
        );
        assert_pids_die(&pids).await;
        let _ = std::fs::remove_file(&pidfile);
    }
}
