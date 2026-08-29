//! The registry itself: task identity, the terminal-commit protocol, the
//! notice queue and the summaries watch. Storage (the session archive, ring
//! files and quota) lives in the sibling modules this one drives.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use coda_core::llm::TaskNoticeOutcome;
use coda_core::tool::CancellationToken;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use crate::archive_dir::{ArchiveDir, ArchiveError};
use crate::manifest::{ExpireReason, OutputDisposition};
use crate::process::{GroupedChild, PIPE_DRAIN_TIMEOUT};
use crate::quota::{
    ArchiveInventory, ExpirationFact, SESSION_QUOTA_BYTES, SessionQuota, scan_inventory,
};
use crate::task_archive::{TaskArchive, TaskRecord};
use crate::task_id::TaskId;

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
        /// Stable id minted when the aggregate is first created and preserved
        /// across merges, so one flood reads as one batch.
        #[serde(default)]
        batch_id: String,
        dropped: Vec<TaskNoticeFact>,
        uncounted: u64,
    },
}

impl TaskNotice {
    /// This notice in the shape the conversation records it: what a client
    /// renders, without the engine's own types leaking into the transcript.
    pub fn outcome(&self) -> TaskNoticeOutcome {
        match self {
            TaskNotice::Task {
                id,
                command,
                status,
                ..
            } => TaskNoticeOutcome::Finished {
                task_id: id.as_str().to_owned(),
                command: command.clone(),
                status: status.describe(),
            },
            TaskNotice::OutputExpired { id, .. } => TaskNoticeOutcome::OutputExpired {
                task_id: id.as_str().to_owned(),
            },
            TaskNotice::Overflow {
                dropped, uncounted, ..
            } => TaskNoticeOutcome::Capped {
                events: dropped.len() as u64 + uncounted,
            },
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

    /// A registry with no store behind it: spawning fails and nothing is
    /// recorded, but the session it belongs to works otherwise. Used when the
    /// archive root cannot be opened.
    pub fn disabled_from(error: String) -> Self {
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
            let record = match archive.open(&id).await {
                Ok(Some(record)) => record,
                Ok(None) => {
                    tracing::warn!(
                        task = id.as_str(),
                        "recoverable task disappeared during reopen"
                    );
                    if let Ok(backend) = self.backend() {
                        backend.quota.block_spawns();
                    }
                    continue;
                }
                Err(error) => {
                    tracing::warn!(task = id.as_str(), error = %error, "recoverable task could not be reopened");
                    if let Ok(backend) = self.backend() {
                        backend.quota.block_spawns();
                    }
                    continue;
                }
            };
            let mut guard = record.lock_commit().await;
            let mut candidate = guard.current().clone();
            candidate.status = TaskStatus::Interrupted {
                at: jiff::Timestamp::now(),
            };
            if let Err(error) = guard.commit(candidate).await {
                tracing::warn!(task = id.as_str(), error = %error, "Running task could not be converted to Interrupted");
                drop(guard);
                if let Ok(backend) = self.backend() {
                    backend.quota.block_spawns();
                }
                continue;
            }
            let status = guard.current().status.clone();
            drop(guard);
            if let Ok(backend) = self.backend()
                && let Err(error) = backend.quota.finalize_terminal(&record).await
            {
                tracing::warn!(task = id.as_str(), error = %error, "Interrupted task output finalize failed");
            }
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
        self.register_task(meta, move |ctx| {
            let group = GroupedChild::spawn(&mut cmd)?;
            Ok(run_process(group, ctx))
        })
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
        self.register_task(meta, move |ctx| Ok(work(ctx))).await
    }

    /// Reserve quota, create the archive record, and register the task. The
    /// registry lock is held across the whole sequence so the capacity check
    /// and the registration are atomic (a concurrent spawn cannot exceed the
    /// limit); this is deadlock-safe because no path holds a per-task commit or
    /// quota lock while waiting for the registry lock. On any failure before
    /// registration no task is published; a process-start failure rolls back
    /// the prepared archive before returning.
    async fn register_task<F, Fut>(&self, meta: TaskMeta, work: F) -> std::io::Result<TaskId>
    where
        F: FnOnce(TaskCtx) -> std::io::Result<Fut>,
        Fut: Future<Output = TaskExit> + Send + 'static,
    {
        let backend = self.backend()?;
        let mut inner = self.inner.lock().await;
        inner.check_capacity()?;

        let outcome = backend.quota.reserve_for_create().await;
        // These facts are already durable even if reservation or archive
        // creation fails, so enqueue them before inspecting the result.
        enqueue_expirations(&mut inner, outcome.expirations);
        let reservation = outcome
            .reservation
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let id = TaskId::new();
        let (record, reservation) = match backend.archive.create(&id, &meta, reservation).await {
            Ok(record) => record,
            Err(error) => {
                // A create error can include a failed half-product cleanup.
                // Stop further growth; the uncommitted reservation rolls back.
                backend.quota.block_spawns();
                return Err(std::io::Error::other(error.to_string()));
            }
        };
        let cancel = CancellationToken::new();
        let entry = Arc::new(TaskEntry {
            record: record.clone(),
            cancel: cancel.clone(),
        });
        let fut = match work(TaskCtx {
            record: record.clone(),
            cancel,
        }) {
            Ok(fut) => fut,
            Err(error) => {
                if let Err(cleanup_error) = backend.archive.discard_created(&record).await {
                    // Keep the reservation charged for files we could not
                    // prove were removed, and close the session to new growth.
                    reservation.commit();
                    backend.quota.block_spawns();
                    return Err(std::io::Error::other(format!(
                        "{error}; background archive rollback failed: {cleanup_error}"
                    )));
                }
                return Err(error);
            }
        };
        reservation.commit();
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
        let expirations = match &self.store {
            Store::Enabled(backend) => backend.quota.take_expirations(),
            Store::Disabled(_) => Vec::new(),
        };
        let mut inner = self.inner.lock().await;
        enqueue_expirations(&mut inner, expirations);
        drain_notices(&mut inner)
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
        for entry in &entries {
            let mut guard = entry.record.lock_commit().await;
            if guard.current().persistence_dirty {
                let candidate = guard.current().clone();
                if let Err(error) = guard.commit(candidate).await {
                    tracing::warn!(
                        task = entry.id().as_str(),
                        error = %error,
                        "shutdown could not persist dirty Failed task state"
                    );
                }
            }
        }
        if let Store::Enabled(backend) = &self.store {
            backend.quota.settle().await;
            backend.archive.settle().await;
        }
        let mut inner = self.inner.lock().await;
        if let Store::Enabled(backend) = &self.store {
            enqueue_expirations(&mut inner, backend.quota.take_expirations());
        }
        let notices = drain_notices(&mut inner);
        // Wake watchers even when nothing changed (e.g. zero tasks) so a
        // keepalive watcher parked on this registry re-checks its entry and
        // can retire once the entry is released.
        inner.publish();
        notices
    }
}

fn enqueue_expirations(inner: &mut RegistryState, expirations: Vec<ExpirationFact>) {
    for fact in expirations {
        inner.push_notice(TaskNotice::OutputExpired {
            id: fact.id,
            expired_at: fact.expired_at,
            reason: fact.reason,
        });
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
    persistence_dirty: bool,
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

    if outcome.persistence_dirty {
        backend.quota.block_spawns();
    }

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
async fn commit_terminal(record: &Arc<TaskRecord>, exit: TaskExit) -> TerminalOutcome {
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
            persistence_dirty: guard.current().persistence_dirty,
            tail,
            stdout_overwritten,
            stderr_overwritten,
        };
    }
    let mut candidate = guard.current().clone();
    candidate.status = intended.clone();
    let commit_error = if flush_ok {
        guard.commit(candidate).await.err()
    } else {
        Some(ArchiveError::Io(std::io::Error::other(
            "background output ring flush failed",
        )))
    };
    let status = if let Some(error) = commit_error {
        let failed = TaskStatus::Failed {
            message: "background output spool save failed".into(),
            at,
        };
        // Settle the runtime exactly once even when no manifest write works.
        // Then retry persisting that same in-memory Failed state best-effort.
        guard
            .fail_in_memory(failed.clone())
            .expect("Running can always degrade to Failed");
        let degraded = guard.current().clone();
        if let Err(retry_error) = guard.commit(degraded).await {
            tracing::warn!(
                task = record.id().as_str(),
                error = %error,
                retry_error = %retry_error,
                "terminal manifest could not be persisted; keeping in-memory Failed state"
            );
        }
        failed
    } else {
        intended
    };
    TerminalOutcome {
        status,
        persistence_dirty: guard.current().persistence_dirty,
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
#[path = "registry_tests.rs"]
mod tests;
