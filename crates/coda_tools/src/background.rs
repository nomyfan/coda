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

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use coda_core::tool::CancellationToken;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
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
/// Tail buffer capacity per stream (stdout / stderr).
const TAIL_BUF_CAP: usize = 512 * 1024;

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
}

impl TaskStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, TaskStatus::Running)
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

/// A completion awaiting delivery. `Task` carries a bounded output tail;
/// `Overflow` aggregates completions evicted from the full-notice window so
/// the terminal *fact* survives even under a flood.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TaskNotice {
    Task {
        id: String,
        command: String,
        description: String,
        status: TaskStatus,
        output_tail: String,
    },
    Overflow {
        dropped: Vec<(String, TaskStatus)>,
        uncounted: u64,
    },
}

impl TaskNotice {
    /// Task ids this notice covers — the dedupe keys for restore.
    pub fn task_ids(&self) -> Vec<String> {
        match self {
            TaskNotice::Task { id, .. } => vec![id.clone()],
            TaskNotice::Overflow { dropped, .. } => {
                dropped.iter().map(|(id, _)| id.clone()).collect()
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
            } => {
                let mut text = format!("Background task {id} finished: {}.", status.describe());
                text.push_str(&format!("\nCommand: {command}"));
                if !description.is_empty() {
                    text.push_str(&format!("\nDescription: {description}"));
                }
                if !output_tail.is_empty() {
                    text.push_str(&format!("\nOutput tail:\n{output_tail}"));
                } else {
                    text.push_str("\n(no output)");
                }
                text
            }
            TaskNotice::Overflow { dropped, uncounted } => {
                let total = dropped.len() as u64 + uncounted;
                let mut text =
                    format!("{total} more background task(s) finished while notices were capped:");
                for (id, status) in dropped {
                    text.push_str(&format!("\n- {id}: {}", status.describe()));
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
}

/// How the task's work future resolved. The process-backed runner reports
/// `Killed` when it tore the process group down in response to cancellation.
#[derive(Debug)]
pub enum TaskExit {
    Exited { code: Option<i32> },
    Killed,
}

/// Bounded tail of one output stream, addressed by absolute offsets so a
/// cursor survives the head being dropped: `start_offset..total_written` is
/// the retained window.
struct TailBuf {
    bytes: Vec<u8>,
    start_offset: u64,
    total_written: u64,
}

impl TailBuf {
    fn new() -> Self {
        TailBuf {
            bytes: Vec::new(),
            start_offset: 0,
            total_written: 0,
        }
    }

    fn append(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        self.total_written += data.len() as u64;
        if self.bytes.len() > TAIL_BUF_CAP {
            let excess = self.bytes.len() - TAIL_BUF_CAP;
            self.bytes.drain(..excess);
            self.start_offset += excess as u64;
        }
    }

    /// Bytes from `cursor` to the end, plus how many bytes between `cursor`
    /// and the window start were lost. The new cursor is `total_written`.
    fn read_from(&self, cursor: u64) -> (Vec<u8>, u64) {
        let lost = self.start_offset.saturating_sub(cursor);
        let from = (cursor.max(self.start_offset) - self.start_offset) as usize;
        (self.bytes[from.min(self.bytes.len())..].to_vec(), lost)
    }

    fn tail_string(&self, limit: usize) -> String {
        let from = self.bytes.len().saturating_sub(limit);
        String::from_utf8_lossy(&self.bytes[from..]).into_owned()
    }
}

struct TaskState {
    status: TaskStatus,
    stdout: TailBuf,
    stderr: TailBuf,
    /// Absolute output offsets consumed by `read` (stdout, stderr).
    cursor: (u64, u64),
}

struct TaskEntry {
    id: String,
    meta: TaskMeta,
    started_at: jiff::Timestamp,
    state: Mutex<TaskState>,
    /// Independent of any turn token: only `kill`/`shutdown` cancel it.
    cancel: CancellationToken,
}

impl TaskEntry {
    fn summary(&self, status: TaskStatus) -> TaskSummary {
        TaskSummary {
            id: self.id.clone(),
            command: self.meta.command.clone(),
            description: self.meta.description.clone(),
            agent_name: self.meta.agent_name.clone(),
            status,
            started_at: self.started_at,
        }
    }
}

/// Handle a task's work future uses to stream output into the registry.
#[derive(Clone)]
pub struct TaskCtx {
    entry: Arc<TaskEntry>,
}

impl TaskCtx {
    /// Cancellation requested via `kill`/`shutdown`. Process-backed work kills
    /// its group and resolves to [`TaskExit::Killed`]; fake work just races it.
    pub fn cancelled(&self) -> CancellationToken {
        self.entry.cancel.clone()
    }

    pub async fn append_stdout(&self, data: &[u8]) {
        self.entry.state.lock().await.stdout.append(data);
    }

    pub async fn append_stderr(&self, data: &[u8]) {
        self.entry.state.lock().await.stderr.append(data);
    }
}

struct RegistryState {
    tasks: HashMap<String, Arc<TaskEntry>>,
    /// Redundant indexes so everything below is answerable while holding this
    /// lock alone — a task's `state` mutex is never taken under it.
    running_count: usize,
    summaries: HashMap<String, TaskSummary>,
    terminal_order: VecDeque<String>,
    monitors: HashMap<String, JoinHandle<()>>,
    notices: Vec<TaskNotice>,
    /// The aggregate slot: never dropped, unlike the full notices feeding it.
    overflow: Option<(Vec<(String, TaskStatus)>, u64)>,
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

    /// Fold terminal facts into the aggregate slot, spilling into the bare
    /// count beyond its capacity.
    fn merge_overflow(&mut self, entries: Vec<(String, TaskStatus)>, uncounted: u64) {
        let (dropped, count) = self.overflow.get_or_insert_with(|| (Vec::new(), 0));
        *count += uncounted;
        for pair in entries {
            if dropped.len() < MAX_OVERFLOW_ENTRIES {
                dropped.push(pair);
            } else {
                *count += 1;
            }
        }
    }

    /// `notices` holds only full `TaskNotice::Task` entries (aggregates live
    /// in the `overflow` slot); the oldest degrades on overflow.
    fn push_notice(&mut self, notice: TaskNotice) {
        self.notices.push(notice);
        if self.notices.len() > MAX_FULL_NOTICES
            && let TaskNotice::Task { id, status, .. } = self.notices.remove(0)
        {
            self.merge_overflow(vec![(id, status)], 0);
        }
    }
}

/// Session-scoped background task registry. Cheap to clone via `Arc`; the
/// owner (hub entry, or the `Session` itself when self-built) is responsible
/// for calling [`shutdown`](Self::shutdown) exactly per the ownership rules
/// in the design doc.
pub struct BackgroundProcesses {
    inner: Arc<Mutex<RegistryState>>,
    summaries_rx: watch::Receiver<Arc<[TaskSummary]>>,
    /// Serializes concurrent `shutdown` calls: the join-before-drain barrier
    /// must hold for every caller, not just the one that drains the monitor
    /// handles first.
    shutdown_gate: Mutex<()>,
}

impl Default for BackgroundProcesses {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundProcesses {
    pub fn new() -> Self {
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
        }
    }

    /// Start `cmd` as a background process task in its own sentinel-pinned
    /// process group. Rejection (closed registry / running limit) is checked
    /// *before* the process starts, so a rejected spawn has no side effects;
    /// only `kill`/`shutdown` terminate it afterwards. Same visibility
    /// guarantee as [`spawn_with`](Self::spawn_with).
    pub async fn spawn(&self, mut cmd: Command, meta: TaskMeta) -> std::io::Result<String> {
        let mut inner = self.inner.lock().await;
        inner.check_capacity()?;
        // Sync and brief; holding the lock keeps the capacity check and the
        // process start atomic.
        let group = GroupedChild::spawn(&mut cmd)?;
        Ok(self.register(&mut inner, meta, move |ctx| run_process(group, ctx)))
    }

    /// Start `work` as a background task. The task is visible in the
    /// summaries (and thus to keepalive watchers) before the id is returned.
    /// Fails when the registry is closed or `MAX_RUNNING` tasks are running.
    pub async fn spawn_with<F, Fut>(&self, meta: TaskMeta, work: F) -> std::io::Result<String>
    where
        F: FnOnce(TaskCtx) -> Fut,
        Fut: Future<Output = TaskExit> + Send + 'static,
    {
        let mut inner = self.inner.lock().await;
        inner.check_capacity()?;
        Ok(self.register(&mut inner, meta, work))
    }

    /// Registers a task under the held registry lock: entry, monitor,
    /// indexes, and the summaries publish — the id is only handed out after
    /// the task is visible to keepalive watchers.
    fn register<F, Fut>(&self, inner: &mut RegistryState, meta: TaskMeta, work: F) -> String
    where
        F: FnOnce(TaskCtx) -> Fut,
        Fut: Future<Output = TaskExit> + Send + 'static,
    {
        let id = format!("bg_{}", uuid::Uuid::new_v4().simple());
        let entry = Arc::new(TaskEntry {
            id: id.clone(),
            meta,
            started_at: jiff::Timestamp::now(),
            state: Mutex::new(TaskState {
                status: TaskStatus::Running,
                stdout: TailBuf::new(),
                stderr: TailBuf::new(),
                cursor: (0, 0),
            }),
            cancel: CancellationToken::new(),
        });

        let fut = work(TaskCtx {
            entry: entry.clone(),
        });
        let monitor = tokio::spawn(monitor_task(self.inner.clone(), entry.clone(), fut));

        inner.tasks.insert(id.clone(), entry.clone());
        inner.running_count += 1;
        inner
            .summaries
            .insert(id.clone(), entry.summary(TaskStatus::Running));
        inner.monitors.insert(id.clone(), monitor);
        inner.publish();
        id
    }

    /// Incremental read: output since the previous read plus current status.
    /// `None` for unknown or reclaimed ids.
    pub async fn read(&self, id: &str) -> Option<TaskRead> {
        let entry = self.inner.lock().await.tasks.get(id).cloned()?;
        let mut state = entry.state.lock().await;
        let (stdout, stdout_lost) = state.stdout.read_from(state.cursor.0);
        let (stderr, stderr_lost) = state.stderr.read_from(state.cursor.1);
        state.cursor = (state.stdout.total_written, state.stderr.total_written);
        Some(TaskRead {
            status: state.status.clone(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            stdout_lost,
            stderr_lost,
        })
    }

    /// Request termination and wait for the monitor's *full* commit — the
    /// published terminal summary, not just the status flip, so an immediate
    /// `take_notices` after returning sees the completion. Idempotent;
    /// returns the settled status, `None` for unknown ids.
    pub async fn kill(&self, id: &str) -> Option<TaskStatus> {
        let entry = self.inner.lock().await.tasks.get(id).cloned()?;
        let mut rx = self.summaries_rx.clone();
        entry.cancel.cancel();
        loop {
            {
                let summaries = rx.borrow_and_update();
                match summaries.iter().find(|summary| summary.id == entry.id) {
                    // Terminal in the published snapshot: the commit (notice
                    // included — publish is its last step) is complete.
                    Some(summary) if !summary.status.is_running() => {
                        return Some(summary.status.clone());
                    }
                    Some(_) => {}
                    // Absent: reclaimed, which only happens post-commit.
                    None => break,
                }
            }
            if rx.changed().await.is_err() {
                break; // registry gone; the entry state is all that's left
            }
        }
        let state = entry.state.lock().await;
        Some(state.status.clone())
    }

    /// Drain accumulated completion notices (the overflow aggregate last).
    pub async fn take_notices(&self) -> Vec<TaskNotice> {
        let mut inner = self.inner.lock().await;
        let mut notices = std::mem::take(&mut inner.notices);
        if let Some((dropped, uncounted)) = inner.overflow.take() {
            notices.push(TaskNotice::Overflow { dropped, uncounted });
        }
        notices
    }

    /// Re-enqueue notices persisted by a previous incarnation: full notices
    /// go ahead of any accumulated since, and a restored aggregate merges
    /// into the overflow slot (`notices` must hold only full entries — a
    /// stray aggregate would poison the demotion path). Once per registry
    /// instance (the caller guarantees once per hub entry).
    pub async fn restore_notices(&self, restored: Vec<TaskNotice>) {
        if restored.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().await;
        let mut fulls = Vec::new();
        for notice in restored {
            match notice {
                TaskNotice::Task { .. } => fulls.push(notice),
                TaskNotice::Overflow { dropped, uncounted } => {
                    inner.merge_overflow(dropped, uncounted);
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
        // committed by the time we collect them.
        for monitor in monitors {
            let _ = monitor.await;
        }
        let mut inner = self.inner.lock().await;
        let mut notices = std::mem::take(&mut inner.notices);
        if let Some((dropped, uncounted)) = inner.overflow.take() {
            notices.push(TaskNotice::Overflow { dropped, uncounted });
        }
        // Wake watchers even when nothing changed (e.g. zero tasks) so a
        // keepalive watcher parked on this registry re-checks its entry and
        // can retire once the entry is released.
        inner.publish();
        notices
    }
}

/// Drives one background process to completion: pumps stdout/stderr into the
/// task's tail buffers and resolves when the leader exits and the pipes are
/// drained. Cancellation (via `kill`/`shutdown`) SIGKILLs the whole group;
/// pipe drains after a group kill are bounded, so a descendant that escaped
/// the group (setsid) and holds a pipe open can't stall the terminal commit.
async fn run_process(mut group: GroupedChild, ctx: TaskCtx) -> TaskExit {
    let mut stdout = group.child.stdout.take().expect("stdout is piped");
    let mut stderr = group.child.stderr.take().expect("stderr is piped");
    let mut out_pump = tokio::spawn({
        let ctx = ctx.clone();
        async move {
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => ctx.append_stdout(&buf[..n]).await,
                }
            }
        }
    });
    let mut err_pump = tokio::spawn({
        let ctx = ctx.clone();
        async move {
            let mut buf = [0u8; 8192];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => ctx.append_stderr(&buf[..n]).await,
                }
            }
        }
    });
    let cancel = ctx.cancelled();

    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            // Kill the whole group, then reap the leader. The pipes hit EOF,
            // letting the pumps flush whatever was produced.
            group.kill_group();
            let _ = group.child.wait().await;
            None
        }
        status = group.child.wait() => Some(status),
    };

    let Some(status) = status else {
        drain_pump(&mut out_pump).await;
        drain_pump(&mut err_pump).await;
        return TaskExit::Killed;
    };

    // Natural leader exit: the pipes usually EOF right away, but a
    // backgrounded child holding an inherited pipe keeps streaming — keep
    // pumping (that output is the task's to report) while racing
    // cancellation, whose group kill ends the stream.
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            group.kill_group();
            drain_pump(&mut out_pump).await;
            drain_pump(&mut err_pump).await;
        }
        _ = async {
            let _ = (&mut out_pump).await;
            let _ = (&mut err_pump).await;
        } => {
            group.disarm();
        }
    }
    TaskExit::Exited {
        code: status.ok().and_then(|s| s.code()),
    }
}

/// Bounded wait for a pipe pump after a group kill: the pipes normally EOF
/// at once, and an escaped descendant holding one open must not stall the
/// terminal commit — on expiry the pump is aborted and its remaining output
/// forfeited.
async fn drain_pump(pump: &mut JoinHandle<()>) {
    if tokio::time::timeout(PIPE_DRAIN_TIMEOUT, &mut *pump)
        .await
        .is_err()
    {
        pump.abort();
    }
}

/// Awaits the task's work and commits the terminal state — the single writer
/// of that transition. Commit order (load-bearing, see the design doc):
/// entry status first, then (under the registry lock) notice enqueue,
/// bookkeeping, and the summaries publish *last*.
async fn monitor_task(
    inner: Arc<Mutex<RegistryState>>,
    entry: Arc<TaskEntry>,
    work: impl Future<Output = TaskExit>,
) {
    let exit = work.await;
    let at = jiff::Timestamp::now();
    let status = match exit {
        TaskExit::Exited { code } => TaskStatus::Exited { code, at },
        TaskExit::Killed => TaskStatus::Killed { at },
    };

    let output_tail = {
        let mut state = entry.state.lock().await;
        state.status = status.clone();
        let tail = state.stdout.tail_string(NOTICE_TAIL_LIMIT);
        if tail.is_empty() {
            state.stderr.tail_string(NOTICE_TAIL_LIMIT)
        } else {
            tail
        }
    };

    let mut inner = inner.lock().await;
    inner.push_notice(TaskNotice::Task {
        id: entry.id.clone(),
        command: entry.meta.command.clone(),
        description: entry.meta.description.clone(),
        status: status.clone(),
        output_tail,
    });
    inner.running_count -= 1;
    inner.terminal_order.push_back(entry.id.clone());
    if inner.terminal_order.len() > MAX_TERMINAL
        && let Some(oldest) = inner.terminal_order.pop_front()
    {
        inner.tasks.remove(&oldest);
        inner.summaries.remove(&oldest);
        // A reclaimed task's monitor has long finished; drop its handle.
        inner.monitors.remove(&oldest);
    }
    if let Some(summary) = inner.summaries.get_mut(&entry.id) {
        summary.status = status;
    }
    inner.publish();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::Notify;

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
        assert!(rx.borrow().iter().any(|s| s.id == id));
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
            ctx.append_stdout(b"done!").await;
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
        let status = reg.kill(&id).await.expect("task known");
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
                ctx.append_stdout(b"first").await;
                g.notified().await;
                // Blow past the buffer cap so the head (including anything
                // unread) is dropped.
                let big = vec![b'x'; TAIL_BUF_CAP + 7];
                ctx.append_stdout(&big).await;
                ctx.cancelled().cancelled().await;
                TaskExit::Killed
            })
            .await
            .unwrap();

        // First read consumes "first" (5 bytes, cursor -> 5).
        let mut seen = String::new();
        while seen.len() < 5 {
            let read = reg.read(&id).await.unwrap();
            seen.push_str(&read.stdout);
            assert_eq!(read.stdout_lost, 0);
            tokio::task::yield_now().await;
        }
        assert_eq!(seen, "first");
        gate.notify_one();

        // Wait until the big write landed, then read: 5 + cap + 7 total
        // written, window holds the last cap bytes → 7 bytes lost.
        let (total, lost, len) = loop {
            let read = reg.read(&id).await.unwrap();
            if !read.stdout.is_empty() || read.stdout_lost > 0 {
                break (5 + TAIL_BUF_CAP + 7, read.stdout_lost, read.stdout.len());
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(lost, 7, "bytes dropped before the read are reported");
        assert_eq!(len, TAIL_BUF_CAP);
        // Cursor is at total_written now: nothing further, nothing repeated.
        let read = reg.read(&id).await.unwrap();
        assert_eq!(read.stdout.len(), 0);
        assert_eq!(read.stdout_lost, 0);
        let _ = total;
        reg.shutdown().await;
    }

    /// Terminal entries beyond MAX_TERMINAL are reclaimed oldest-first;
    /// reclaimed ids read as None.
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
                    .any(|s| s.id == id && !s.status.is_running());
                let gone = i > 0 && !rx.borrow().iter().any(|s| s.id == id);
                if done || gone {
                    break;
                }
                rx.changed().await.unwrap();
            }
        }
        let first = first_id.unwrap();
        assert!(
            reg.read(&first).await.is_none(),
            "oldest terminal task is reclaimed"
        );
        // Its notice still exists — reclamation frees buffers, not facts.
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
                    .any(|s| s.id == id && !s.status.is_running())
                    || !rx.borrow().iter().any(|s| s.id == id);
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
                TaskNotice::Overflow { dropped, uncounted } => Some((dropped.len(), *uncounted)),
                _ => None,
            })
            .collect();
        assert_eq!(overflow, vec![(3, 0)]);
        reg.shutdown().await;
    }

    /// A restored aggregate merges into the overflow slot — never into the
    /// full-notice queue, where the demotion path only expects full notices.
    #[tokio::test]
    async fn restore_merges_persisted_aggregate_into_overflow_slot() {
        let reg = BackgroundProcesses::new();
        let killed = TaskStatus::Killed {
            at: jiff::Timestamp::now(),
        };
        reg.restore_notices(vec![
            TaskNotice::Task {
                id: "bg_old".into(),
                command: "old".into(),
                description: String::new(),
                status: killed.clone(),
                output_tail: String::new(),
            },
            TaskNotice::Overflow {
                dropped: vec![("bg_x".into(), killed)],
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
                    .any(|s| s.id == id && !s.status.is_running())
                    || !rx.borrow().iter().any(|s| s.id == id);
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
                TaskNotice::Overflow { dropped, uncounted } => Some((dropped, *uncounted)),
                _ => None,
            })
            .expect("aggregate present");
        let dropped_ids: Vec<&str> = dropped.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(dropped_ids, vec!["bg_x", "bg_old"]);
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
        reg.restore_notices(vec![TaskNotice::Task {
            id: "bg_old".into(),
            command: "old".into(),
            description: String::new(),
            status: TaskStatus::Killed {
                at: jiff::Timestamp::now(),
            },
            output_tail: String::new(),
        }])
        .await;
        let notices = reg.take_notices().await;
        let ids: Vec<Vec<String>> = notices.iter().map(|n| n.task_ids()).collect();
        assert_eq!(ids, vec![vec!["bg_old".to_string()], vec![id]]);
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
                let read = reg.read(&id).await.expect("task known");
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
        let last = reg.read(&id).await.unwrap();
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

        let status = reg.kill(&id).await.expect("task known");
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
