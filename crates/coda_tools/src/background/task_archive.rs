//! Session-owned task archive: the per-task manifest, the guarded commit that
//! linearizes every persisted mutation, and lazy reopen of a terminal task by
//! id (design: components `TaskOutputFiles` / `TaskArchive`).
//!
//! The **commit lock** on each [`TaskRecord`] is the persistence linearization
//! point. `task_output`, terminal commit, release, reopen conversion and quota
//! eviction each take it and run the whole
//! `read-state → I/O → temp write → atomic rename → memory-commit` sequence
//! under it, so status stays monotone (a terminal state is never overwritten),
//! read cursors only advance, and two concurrent reads never overlap. Ring
//! *appends* deliberately do **not** take this lock — they hold only the
//! per-stream `DiskTail` lock — because appends run while `Running` and no
//! terminal manifest is written until the pumps have finished.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex as StdMutex, Weak};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, watch};

use super::archive_dir::{ArchiveDir, ArchiveError, ArchiveFileName};
use super::disk_tail::{DiskTail, LAYOUT_VERSION};
use super::manifest::{MANIFEST_VERSION, OutputDisposition, StreamManifest, TaskOutputManifest};
use super::quota::{CreateReservationLease, QuotaReservation};
use super::task_id::TaskId;
use super::{TaskMeta, TaskStatus};

/// Per-stream ring capacity for a fresh task (512 KiB). Internal config, not a
/// wire contract; a file always reopens at its manifest capacity, not this.
pub const DEFAULT_STREAM_CAPACITY: u64 = 512 * 1024;

/// Largest `meta.json` we will read; a larger file is treated as corrupt
/// without materialising its contents.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// The mutable, persisted slice of a task's state — everything the commit lock
/// serializes. Ring `start_offset`/`total_written` are owned by `DiskTail` and
/// snapshotted at commit time, so they are not duplicated here.
#[derive(Debug, Clone)]
pub struct TaskPersistentState {
    pub status: TaskStatus,
    pub stdout_cursor: u64,
    pub stderr_cursor: u64,
    pub stdout_carry: Vec<u8>,
    pub stderr_carry: Vec<u8>,
    pub disposition: OutputDisposition,
    /// The runtime terminal state is newer than the durable manifest. Set only
    /// by the final in-memory Failed degradation and cleared by any successful
    /// manifest commit.
    pub persistence_dirty: bool,
}

/// The two ring streams of a live (Retained) task. Appends clone the inner
/// `Arc<DiskTail>` and never touch the commit lock.
pub struct TaskOutputFiles {
    pub stdout: Arc<DiskTail>,
    pub stderr: Arc<DiskTail>,
}

impl TaskOutputFiles {
    /// Flush both rings — the durability barrier a terminal/release manifest
    /// must clear before it records a new logical range.
    pub async fn flush(&self) -> std::io::Result<()> {
        self.stdout.flush().await?;
        self.stderr.flush().await
    }
}

/// One archived task: immutable identity plus the guarded mutable state and the
/// ring handles. The same `TaskId` resolves to the same live record (and thus
/// the same commit lock) process-wide via [`TaskArchive`]'s weak index.
pub struct TaskRecord {
    id: TaskId,
    meta: TaskMeta,
    started_at: jiff::Timestamp,
    /// Immutable per-stream capacities (stdout, stderr).
    capacities: (u64, u64),
    task_dir: ArchiveDir,
    files: TaskOutputFiles,
    commit: Arc<Mutex<TaskPersistentState>>,
    activity: Arc<ArchiveActivity>,
    #[cfg(test)]
    commit_pause: StdMutex<Option<CommitPause>>,
}

impl TaskRecord {
    pub fn id(&self) -> &TaskId {
        &self.id
    }

    pub fn meta(&self) -> &TaskMeta {
        &self.meta
    }

    pub fn started_at(&self) -> jiff::Timestamp {
        self.started_at
    }

    /// Immutable per-stream ring capacities (stdout, stderr).
    pub fn capacities(&self) -> (u64, u64) {
        self.capacities
    }

    pub fn files(&self) -> &TaskOutputFiles {
        &self.files
    }

    /// Acquire the persistence linearization guard. Every persisted mutation of
    /// this task goes through it; there is no commit path that bypasses it.
    pub async fn lock_commit(self: &Arc<Self>) -> TaskCommitGuard {
        let state = self.commit.clone().lock_owned().await;
        TaskCommitGuard {
            record: self.clone(),
            state: Some(state),
        }
    }

    #[cfg(test)]
    fn pause_next_commit(
        &self,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        *self.commit_pause.lock().unwrap() = Some(CommitPause {
            entered: entered_tx,
            release: release_rx,
        });
        (entered_rx, release_tx)
    }

    /// Build the full manifest for a candidate state, snapshotting each ring's
    /// current logical range (callers flush first for a terminal range).
    async fn build_manifest(&self, state: &TaskPersistentState) -> TaskOutputManifest {
        let (so_start, so_total) = self.files.stdout.logical_range().await;
        let (se_start, se_total) = self.files.stderr.logical_range().await;
        TaskOutputManifest {
            manifest_version: MANIFEST_VERSION,
            id: self.id.clone(),
            command: self.meta.command.clone(),
            description: self.meta.description.clone(),
            agent_name: self.meta.agent_name.clone(),
            started_at: self.started_at,
            terminal_at: state.status.terminal_at(),
            status: state.status.clone(),
            stdout: StreamManifest {
                layout_version: LAYOUT_VERSION,
                capacity: self.capacities.0,
                start_offset: so_start,
                total_written: so_total,
                read_cursor: state.stdout_cursor,
                utf8_carry: state.stdout_carry.clone(),
            },
            stderr: StreamManifest {
                layout_version: LAYOUT_VERSION,
                capacity: self.capacities.1,
                start_offset: se_start,
                total_written: se_total,
                read_cursor: state.stderr_cursor,
                utf8_carry: state.stderr_carry.clone(),
            },
            output: state.disposition.clone(),
        }
    }
}

/// Held guard over a task's persisted state. `current()` reads the committed
/// snapshot; `commit()` validates the monotonic invariants and atomically saves
/// the manifest before swapping in the new memory state.
pub struct TaskCommitGuard {
    record: Arc<TaskRecord>,
    state: Option<tokio::sync::OwnedMutexGuard<TaskPersistentState>>,
}

struct ArchiveActivity {
    count: watch::Sender<usize>,
}

struct ArchiveActivityGuard {
    activity: Arc<ArchiveActivity>,
}

impl ArchiveActivity {
    fn begin(activity: &Arc<Self>) -> ArchiveActivityGuard {
        activity.count.send_modify(|count| *count += 1);
        ArchiveActivityGuard {
            activity: activity.clone(),
        }
    }

    async fn settle(activity: &Arc<Self>) {
        let mut count = activity.count.subscribe();
        loop {
            if *count.borrow_and_update() == 0 {
                return;
            }
            if count.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Drop for ArchiveActivityGuard {
    fn drop(&mut self) {
        self.activity.count.send_modify(|count| *count -= 1);
    }
}

impl TaskCommitGuard {
    pub fn current(&self) -> &TaskPersistentState {
        self.state.as_deref().expect("commit guard owns state")
    }

    pub fn record(&self) -> &TaskRecord {
        &self.record
    }

    /// Validate `current → candidate` against the monotonic invariants, save
    /// the manifest atomically, and only then replace the in-memory state. On
    /// any error the memory state is untouched.
    pub async fn commit(&mut self, candidate: TaskPersistentState) -> Result<(), ArchiveError> {
        let mut candidate = candidate;
        candidate.persistence_dirty = false;
        check_transition(self.current(), &candidate)?;
        let manifest = self.record.build_manifest(&candidate).await;
        validate_cursor_bounds(&manifest)?;
        let task_dir = self.record.task_dir.clone();
        #[cfg(test)]
        let pause = self.record.commit_pause.lock().unwrap().take();
        let mut state = self.state.take().expect("commit guard owns state");
        let activity = ArchiveActivity::begin(&self.record.activity);
        let transaction = tokio::spawn(async move {
            let _activity = activity;
            let save = tokio::task::spawn_blocking(move || {
                save_manifest(&task_dir, &manifest)?;
                #[cfg(test)]
                if let Some(pause) = pause {
                    let _ = pause.entered.send(());
                    let _ = pause.release.recv();
                }
                Ok(())
            })
            .await
            .map_err(join_err)
            .and_then(|result| result);
            if save.is_ok() {
                *state = candidate;
            }
            (state, save)
        });
        match transaction.await {
            Ok((state, result)) => {
                self.state = Some(state);
                result
            }
            Err(error) => {
                self.state = Some(self.record.commit.clone().lock_owned().await);
                Err(join_err(error))
            }
        }
    }

    /// Record the terminal failure in memory when the archive cannot persist
    /// any terminal manifest. This is the final degradation boundary: runtime
    /// lifecycle state must still settle even though crash recovery cannot be
    /// made reliable. A later `commit(current().clone())` may retry the save.
    pub fn fail_in_memory(&mut self, status: TaskStatus) -> Result<(), ArchiveError> {
        if !matches!(status, TaskStatus::Failed { .. }) {
            return Err(ArchiveError::corrupt(
                "in-memory persistence degradation must be Failed",
            ));
        }
        let mut candidate = self.current().clone();
        candidate.status = status;
        candidate.persistence_dirty = true;
        check_transition(self.current(), &candidate)?;
        *self.state.as_deref_mut().expect("commit guard owns state") = candidate;
        Ok(())
    }

    /// Delete the two ring files for a task whose disposition has just been
    /// committed as `Consumed`/`Expired`. Manifest-first ordering means this
    /// runs only after the unreadable disposition is durable.
    pub async fn delete_rings(&self) -> Result<(), ArchiveError> {
        let task_dir = self.record.task_dir.clone();
        tokio::task::spawn_blocking(move || {
            task_dir.unlink(ArchiveFileName::StdoutRing)?;
            task_dir.unlink(ArchiveFileName::StderrRing)?;
            Ok::<(), ArchiveError>(())
        })
        .await
        .map_err(join_err)?
    }
}

#[cfg(test)]
struct CommitPause {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

/// Monotonic-transition invariants (design: "manifest 提交不变量").
fn check_transition(
    current: &TaskPersistentState,
    candidate: &TaskPersistentState,
) -> Result<(), ArchiveError> {
    // Terminal status is immutable; Running may advance to any terminal.
    if !current.status.is_running() && candidate.status != current.status {
        return Err(ArchiveError::corrupt(
            "terminal status cannot change once committed",
        ));
    }
    // Cursors only advance.
    if candidate.stdout_cursor < current.stdout_cursor
        || candidate.stderr_cursor < current.stderr_cursor
    {
        return Err(ArchiveError::corrupt("read cursor cannot move backwards"));
    }
    // Disposition only advances Retained → (Consumed | Expired), never back.
    match (&current.disposition, &candidate.disposition) {
        (OutputDisposition::Retained, _) => {}
        (a, b) if a == b => {}
        _ => {
            return Err(ArchiveError::corrupt(
                "output disposition cannot change once terminal",
            ));
        }
    }
    Ok(())
}

/// Cursor must never exceed the total bytes the ring reports.
fn validate_cursor_bounds(m: &TaskOutputManifest) -> Result<(), ArchiveError> {
    if m.stdout.read_cursor > m.stdout.total_written
        || m.stderr.read_cursor > m.stderr.total_written
    {
        return Err(ArchiveError::corrupt("read cursor exceeds total_written"));
    }
    Ok(())
}

/// The lazy index mapping a `TaskId` to its single live record, so concurrent
/// opens of the same id share one commit lock. Inventory never inserts here;
/// only an active create or an explicit `open(id)` materialises a record.
#[derive(Clone)]
pub struct TaskArchive {
    root: ArchiveDir,
    index: Arc<StdMutex<HashMap<TaskId, Weak<TaskRecord>>>>,
    activity: Arc<ArchiveActivity>,
    #[cfg(test)]
    fail_next_initial_manifest: Arc<AtomicBool>,
    #[cfg(test)]
    create_pause: Arc<StdMutex<Option<CreatePause>>>,
    #[cfg(test)]
    fail_next_discard: Arc<AtomicBool>,
}

impl TaskArchive {
    pub fn new(root: ArchiveDir) -> Self {
        TaskArchive {
            root,
            index: Arc::new(StdMutex::new(HashMap::new())),
            activity: Arc::new(ArchiveActivity {
                count: watch::channel(0).0,
            }),
            #[cfg(test)]
            fail_next_initial_manifest: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            create_pause: Arc::new(StdMutex::new(None)),
            #[cfg(test)]
            fail_next_discard: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn root(&self) -> &ArchiveDir {
        &self.root
    }

    #[cfg(test)]
    pub(crate) fn fail_next_initial_manifest(&self) {
        self.fail_next_initial_manifest
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn pause_next_create(&self) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *self.create_pause.lock().unwrap() = Some(CreatePause {
            entered: entered.clone(),
            release: release.clone(),
        });
        (entered, release)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_discard(&self) {
        self.fail_next_discard.store(true, Ordering::SeqCst);
    }

    /// Create a task's private `0700` directory and two `0600` ring files, and
    /// register the live record. All-or-nothing: on any failure the partial
    /// directory is removed and no record is returned.
    pub(crate) async fn create(
        &self,
        id: &TaskId,
        meta: &TaskMeta,
        reservation: QuotaReservation,
    ) -> Result<(Arc<TaskRecord>, QuotaReservation), ArchiveError> {
        let lease = reservation
            .prepare_create(2 * DEFAULT_STREAM_CAPACITY)
            .map_err(|error| ArchiveError::corrupt(error.to_string()))?;
        let record = self.create_transaction(id, meta, Some(lease)).await?;
        Ok((record, reservation))
    }

    #[cfg(test)]
    pub(crate) async fn create_unreserved(
        &self,
        id: &TaskId,
        meta: &TaskMeta,
    ) -> Result<Arc<TaskRecord>, ArchiveError> {
        self.create_transaction(id, meta, None).await
    }

    async fn create_transaction(
        &self,
        id: &TaskId,
        meta: &TaskMeta,
        reservation: Option<CreateReservationLease>,
    ) -> Result<Arc<TaskRecord>, ArchiveError> {
        let archive = self.clone();
        let id = id.clone();
        let meta = meta.clone();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let activity = ArchiveActivity::begin(&self.activity);
        tokio::spawn(async move {
            let _activity = activity;
            let result = archive.create_inner(&id, &meta).await;
            let created = result.as_ref().ok().cloned();
            let create_cleanup_failed = result
                .as_ref()
                .err()
                .is_some_and(|failure| failure.cleanup_failed);
            let diagnostic = result
                .as_ref()
                .err()
                .map(|failure| failure.error.to_string());
            let acknowledged = result_tx.send(result).is_ok() && ack_rx.await.is_ok();
            if create_cleanup_failed {
                let error = ArchiveError::corrupt(
                    diagnostic.unwrap_or_else(|| "task create cleanup failed".into()),
                );
                tracing::error!(task = id.as_str(), error = %error, "task create cleanup failed");
                if let Some(reservation) = reservation {
                    reservation.block_and_commit(&error);
                }
                return;
            }
            if acknowledged {
                return;
            }

            let cleanup_error = if let Some(record) = created {
                archive.discard_created(&record).await.err()
            } else {
                None
            };
            if let Some(error) = cleanup_error {
                tracing::error!(task = id.as_str(), error = %error, "detached task create cleanup failed");
                if let Some(reservation) = reservation {
                    reservation.block_and_commit(&error);
                }
            }
        });

        let result = result_rx
            .await
            .map_err(|_| ArchiveError::corrupt("task create transaction stopped"))?;
        ack_tx
            .send(())
            .map_err(|_| ArchiveError::corrupt("task create delivery stopped"))?;
        result.map_err(|failure| failure.error)
    }

    async fn create_inner(
        &self,
        id: &TaskId,
        meta: &TaskMeta,
    ) -> Result<Arc<TaskRecord>, CreateFailure> {
        #[cfg(test)]
        let pause = { self.create_pause.lock().unwrap().take() };
        #[cfg(test)]
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
        let root = self.root.clone();
        let id_owned = id.clone();
        let (task_dir, stdout_file, stderr_file) = tokio::task::spawn_blocking(move || {
            let task_dir = root.create_dir(&id_owned).map_err(CreateFailure::new)?;
            let build = (|| {
                let out = task_dir.create_file(ArchiveFileName::StdoutRing)?;
                let err = task_dir.create_file(ArchiveFileName::StderrRing)?;
                Ok::<_, ArchiveError>((out, err))
            })();
            match build {
                Ok((out, err)) => Ok((task_dir, out, err)),
                Err(e) => {
                    let cleanup = rollback_created_task_blocking(&root, &id_owned, &task_dir);
                    Err(CreateFailure::after_cleanup(e, cleanup))
                }
            }
        })
        .await
        .map_err(|error| CreateFailure::new(join_err(error)))??;

        let stdout = match DiskTail::create(stdout_file, DEFAULT_STREAM_CAPACITY) {
            Ok(stdout) => Arc::new(stdout),
            Err(e) => {
                let cleanup =
                    rollback_created_task(self.root.clone(), id.clone(), task_dir.clone()).await;
                return Err(CreateFailure::after_cleanup(e.into(), cleanup));
            }
        };
        let stderr = match DiskTail::create(stderr_file, DEFAULT_STREAM_CAPACITY) {
            Ok(stderr) => Arc::new(stderr),
            Err(e) => {
                let cleanup =
                    rollback_created_task(self.root.clone(), id.clone(), task_dir.clone()).await;
                return Err(CreateFailure::after_cleanup(e.into(), cleanup));
            }
        };
        let state = TaskPersistentState {
            status: TaskStatus::Running,
            stdout_cursor: 0,
            stderr_cursor: 0,
            stdout_carry: Vec::new(),
            stderr_carry: Vec::new(),
            disposition: OutputDisposition::Retained,
            persistence_dirty: false,
        };
        let record = Arc::new(TaskRecord {
            id: id.clone(),
            meta: meta.clone(),
            started_at: jiff::Timestamp::now(),
            capacities: (DEFAULT_STREAM_CAPACITY, DEFAULT_STREAM_CAPACITY),
            task_dir,
            files: TaskOutputFiles { stdout, stderr },
            commit: Arc::new(Mutex::new(state)),
            activity: self.activity.clone(),
            #[cfg(test)]
            commit_pause: StdMutex::new(None),
        });

        // Persist the initial Running manifest before handing out the record.
        {
            let mut guard = record.lock_commit().await;
            let candidate = guard.current().clone();
            #[cfg(test)]
            let commit = if self
                .fail_next_initial_manifest
                .swap(false, Ordering::SeqCst)
            {
                Err(ArchiveError::Io(std::io::Error::other(
                    "injected initial manifest failure",
                )))
            } else {
                guard.commit(candidate).await
            };
            #[cfg(not(test))]
            let commit = guard.commit(candidate).await;
            if let Err(e) = commit {
                drop(guard);
                let cleanup =
                    rollback_created_task(self.root.clone(), id.clone(), record.task_dir.clone())
                        .await;
                return Err(CreateFailure::after_cleanup(e, cleanup));
            }
        }

        self.index
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::downgrade(&record));
        Ok(record)
    }

    pub async fn settle(&self) {
        ArchiveActivity::settle(&self.activity).await;
    }

    /// Roll back a freshly created record that was never published because
    /// starting its process failed. The caller still owns the quota guard.
    pub async fn discard_created(&self, record: &TaskRecord) -> Result<(), ArchiveError> {
        self.index.lock().unwrap().remove(record.id());
        #[cfg(test)]
        if self.fail_next_discard.swap(false, Ordering::SeqCst) {
            return Err(ArchiveError::Io(std::io::Error::other(
                "injected detached create cleanup failure",
            )));
        }
        rollback_created_task(
            self.root.clone(),
            record.id().clone(),
            record.task_dir.clone(),
        )
        .await
    }

    /// Open an archived task by id, materialising a single live record. Returns
    /// `Ok(None)` only for an unknown id; a present-but-corrupt task is an
    /// `Err`, never a silent empty task.
    pub async fn open(&self, id: &TaskId) -> Result<Option<Arc<TaskRecord>>, ArchiveError> {
        // Fast path: an already-live record shares its commit lock.
        if let Some(existing) = self.index.lock().unwrap().get(id).and_then(Weak::upgrade) {
            return Ok(Some(existing));
        }

        let root = self.root.clone();
        let id_owned = id.clone();
        let loaded = tokio::task::spawn_blocking(move || load_task_dir(&root, &id_owned))
            .await
            .map_err(join_err)??;
        let Some((task_dir, manifest)) = loaded else {
            return Ok(None);
        };

        let record = self.reopen_record(id, task_dir, manifest).await?;
        // get-or-insert atomically so a concurrent open cannot mint a second
        // commit lock for the same id.
        let mut index = self.index.lock().unwrap();
        if let Some(existing) = index.get(id).and_then(Weak::upgrade) {
            return Ok(Some(existing));
        }
        index.insert(id.clone(), Arc::downgrade(&record));
        Ok(Some(record))
    }

    /// Reconstruct a `TaskRecord` from a validated manifest, reopening the ring
    /// files (only when the disposition says they should still exist).
    async fn reopen_record(
        &self,
        id: &TaskId,
        task_dir: ArchiveDir,
        manifest: TaskOutputManifest,
    ) -> Result<Arc<TaskRecord>, ArchiveError> {
        let rings_present = manifest.output.rings_present();
        let td = task_dir.clone();
        let stdout_m = manifest.stdout.clone();
        let stderr_m = manifest.stderr.clone();
        let (stdout, stderr) = tokio::task::spawn_blocking(move || {
            let open_ring =
                |name: ArchiveFileName, m: &StreamManifest| -> Result<DiskTail, ArchiveError> {
                    let file = td.open_file(name, true)?;
                    DiskTail::reopen(file, m.capacity, m.start_offset, m.total_written)
                        .map_err(Into::into)
                };
            if rings_present {
                let out = open_ring(ArchiveFileName::StdoutRing, &stdout_m)?;
                let err = open_ring(ArchiveFileName::StderrRing, &stderr_m)?;
                Ok::<_, ArchiveError>((out, err))
            } else {
                // Rings deleted: expose detached streams keeping the recorded
                // range so reads gate on disposition, not on I/O.
                let out = DiskTail::detached(
                    stdout_m.capacity,
                    stdout_m.start_offset,
                    stdout_m.total_written,
                )?;
                let err = DiskTail::detached(
                    stderr_m.capacity,
                    stderr_m.start_offset,
                    stderr_m.total_written,
                )?;
                Ok((out, err))
            }
        })
        .await
        .map_err(join_err)??;

        let state = TaskPersistentState {
            status: manifest.status.clone(),
            stdout_cursor: manifest.stdout.read_cursor,
            stderr_cursor: manifest.stderr.read_cursor,
            stdout_carry: manifest.stdout.utf8_carry.clone(),
            stderr_carry: manifest.stderr.utf8_carry.clone(),
            disposition: manifest.output.clone(),
            persistence_dirty: false,
        };
        Ok(Arc::new(TaskRecord {
            id: id.clone(),
            meta: TaskMeta {
                command: manifest.command,
                description: manifest.description,
                agent_name: manifest.agent_name,
            },
            started_at: manifest.started_at,
            capacities: (manifest.stdout.capacity, manifest.stderr.capacity),
            task_dir,
            files: TaskOutputFiles {
                stdout: Arc::new(stdout),
                stderr: Arc::new(stderr),
            },
            commit: Arc::new(Mutex::new(state)),
            activity: self.activity.clone(),
            #[cfg(test)]
            commit_pause: StdMutex::new(None),
        }))
    }
}

#[cfg(test)]
struct CreatePause {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

struct CreateFailure {
    error: ArchiveError,
    cleanup_failed: bool,
}

impl CreateFailure {
    fn new(error: ArchiveError) -> Self {
        Self {
            error,
            cleanup_failed: false,
        }
    }

    fn after_cleanup(error: ArchiveError, cleanup: Result<(), ArchiveError>) -> Self {
        match cleanup {
            Ok(()) => Self::new(error),
            Err(cleanup) => Self {
                error: ArchiveError::corrupt(format!(
                    "{error}; task create rollback failed: {cleanup}"
                )),
                cleanup_failed: true,
            },
        }
    }
}

async fn rollback_created_task(
    root: ArchiveDir,
    id: TaskId,
    task_dir: ArchiveDir,
) -> Result<(), ArchiveError> {
    tokio::task::spawn_blocking(move || rollback_created_task_blocking(&root, &id, &task_dir))
        .await
        .map_err(join_err)?
}

fn rollback_created_task_blocking(
    root: &ArchiveDir,
    id: &TaskId,
    task_dir: &ArchiveDir,
) -> Result<(), ArchiveError> {
    let mut first_error = None;
    for name in [
        ArchiveFileName::MetaTmp,
        ArchiveFileName::Meta,
        ArchiveFileName::StdoutRing,
        ArchiveFileName::StderrRing,
    ] {
        if let Err(error) = task_dir.unlink(name)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    if let Err(error) = root.remove_dir(id)
        && first_error.is_none()
    {
        first_error = Some(error);
    }
    first_error.map_or(Ok(()), Err)
}

/// Load and validate a task directory's manifest (blocking). `Ok(None)` for an
/// absent directory; `Err` for a present-but-invalid one.
fn load_task_dir(
    root: &ArchiveDir,
    id: &TaskId,
) -> Result<Option<(ArchiveDir, TaskOutputManifest)>, ArchiveError> {
    let task_dir = match root.open_dir(id) {
        Ok(d) => d,
        Err(ArchiveError::Io(e)) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
        Err(e) => return Err(e),
    };
    let manifest = read_manifest(&task_dir)?;
    validate_manifest(id, &manifest)?;
    Ok(Some((task_dir, manifest)))
}

/// Read and size-cap `meta.json` (blocking).
pub(crate) fn read_manifest(task_dir: &ArchiveDir) -> Result<TaskOutputManifest, ArchiveError> {
    let mut file = task_dir.open_file(ArchiveFileName::Meta, false)?;
    let len = file.metadata()?.len();
    if len > MAX_MANIFEST_BYTES {
        return Err(ArchiveError::corrupt(format!(
            "meta.json is {len} bytes, over the {MAX_MANIFEST_BYTES} cap"
        )));
    }
    let mut buf = Vec::with_capacity(len as usize);
    file.read_to_end(&mut buf)?;
    serde_json::from_slice(&buf)
        .map_err(|e| ArchiveError::corrupt(format!("meta.json parse error: {e}")))
}

/// Cross-field manifest validation shared by open and inventory.
pub(crate) fn validate_manifest(
    id: &TaskId,
    manifest: &TaskOutputManifest,
) -> Result<(), ArchiveError> {
    if manifest.manifest_version != MANIFEST_VERSION {
        return Err(ArchiveError::corrupt(format!(
            "unsupported manifest_version {}",
            manifest.manifest_version
        )));
    }
    if &manifest.id != id {
        return Err(ArchiveError::corrupt(
            "manifest id does not match its directory name",
        ));
    }
    manifest.stdout.validate().map_err(ArchiveError::corrupt)?;
    manifest.stderr.validate().map_err(ArchiveError::corrupt)?;

    let terminal = !manifest.status.is_running();
    if terminal != manifest.terminal_at.is_some() {
        return Err(ArchiveError::corrupt(
            "terminal_at presence disagrees with status",
        ));
    }
    match &manifest.output {
        OutputDisposition::Retained => {
            if let TaskStatus::Running = manifest.status {
                // Running is always Retained — fine.
            }
        }
        OutputDisposition::Consumed { .. } => {
            if !terminal {
                return Err(ArchiveError::corrupt("Consumed requires a terminal status"));
            }
            if !manifest.stdout.fully_consumed() || !manifest.stderr.fully_consumed() {
                return Err(ArchiveError::corrupt(
                    "Consumed requires both cursors at total_written",
                ));
            }
            if !manifest.stdout.utf8_carry.is_empty() || !manifest.stderr.utf8_carry.is_empty() {
                return Err(ArchiveError::corrupt("Consumed requires empty utf8_carry"));
            }
        }
        OutputDisposition::Expired { .. } => {
            if !terminal {
                return Err(ArchiveError::corrupt("Expired requires a terminal status"));
            }
        }
    }
    Ok(())
}

/// Atomically persist a manifest: temp write + fsync + rename (blocking).
fn save_manifest(task_dir: &ArchiveDir, manifest: &TaskOutputManifest) -> Result<(), ArchiveError> {
    let json = serde_json::to_vec_pretty(manifest)
        .map_err(|e| ArchiveError::corrupt(format!("manifest serialize error: {e}")))?;
    // Clear any crash-leftover temp before O_EXCL create.
    task_dir.unlink(ArchiveFileName::MetaTmp)?;
    let mut file = task_dir.create_file(ArchiveFileName::MetaTmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    task_dir.rename(ArchiveFileName::MetaTmp, ArchiveFileName::Meta)?;
    Ok(())
}

fn join_err(err: tokio::task::JoinError) -> ArchiveError {
    ArchiveError::corrupt(format!("archive worker failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::quota::{QuotaError, SESSION_QUOTA_BYTES, SessionQuota, scan_inventory};

    fn meta() -> TaskMeta {
        TaskMeta {
            command: "echo hi".into(),
            description: "d".into(),
            agent_name: "coda".into(),
        }
    }

    fn root() -> (tempfile::TempDir, TaskArchive) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ArchiveDir::open_or_create_root(&tmp.path().join("background/tasks")).unwrap();
        (tmp, TaskArchive::new(dir))
    }

    #[tokio::test]
    async fn create_persists_running_manifest_and_reopens() {
        let (_tmp, archive) = root();
        let id = TaskId::new();
        let record = archive.create_unreserved(&id, &meta()).await.unwrap();
        record.files().stdout.append(b"hello").await.unwrap();
        record.files().stdout.flush().await.unwrap();
        // Commit the advanced range (Running cursor update).
        {
            let mut g = record.lock_commit().await;
            let mut cand = g.current().clone();
            cand.stdout_cursor = 5;
            g.commit(cand).await.unwrap();
        }
        drop(record);

        // Reopen from disk: same id, Running, cursor preserved, output readable.
        let reopened = archive.open(&id).await.unwrap().expect("task present");
        let g = reopened.lock_commit().await;
        assert_eq!(g.current().stdout_cursor, 5);
        assert!(matches!(g.current().status, TaskStatus::Running));
        let chunk = reopened.files().stdout.read_from(0, 64).await.unwrap();
        assert_eq!(chunk.bytes, b"hello");
    }

    #[tokio::test]
    async fn initial_manifest_failure_removes_partial_task() {
        let (_tmp, archive) = root();
        archive.fail_next_initial_manifest();
        let id = TaskId::new();
        assert!(archive.create_unreserved(&id, &meta()).await.is_err());
        assert!(archive.root().open_dir(&id).is_err());
        assert_eq!(archive.root().entries().unwrap().count(), 0);
    }

    #[tokio::test]
    async fn cancelled_manifest_commit_finishes_disk_and_memory_together() {
        let (_tmp, archive) = root();
        let id = TaskId::new();
        let record = archive.create_unreserved(&id, &meta()).await.unwrap();
        let (entered, release) = record.pause_next_commit();
        let task_record = record.clone();
        let commit = tokio::spawn(async move {
            let mut guard = task_record.lock_commit().await;
            let mut candidate = guard.current().clone();
            candidate.status = TaskStatus::Exited {
                code: Some(0),
                at: jiff::Timestamp::now(),
            };
            guard.commit(candidate).await
        });
        tokio::task::spawn_blocking(move || entered.recv().unwrap())
            .await
            .unwrap();
        commit.abort();
        let cancelled = tokio::time::timeout(std::time::Duration::from_millis(100), commit)
            .await
            .expect("cancelling commit blocked the Tokio worker")
            .unwrap_err();
        assert!(cancelled.is_cancelled());
        release.send(()).unwrap();

        assert!(matches!(
            record.lock_commit().await.current().status,
            TaskStatus::Exited { .. }
        ));
        let reopened = TaskArchive::new(archive.root().clone());
        let disk = reopened.open(&id).await.unwrap().unwrap();
        assert!(matches!(
            disk.lock_commit().await.current().status,
            TaskStatus::Exited { .. }
        ));
    }

    #[tokio::test]
    async fn cancelled_create_transaction_cleans_delivered_record() {
        let (_tmp, archive) = root();
        let (entered, release) = archive.pause_next_create();
        let create_archive = archive.clone();
        let id = TaskId::new();
        let create_id = id.clone();
        let create =
            tokio::spawn(
                async move { create_archive.create_unreserved(&create_id, &meta()).await },
            );
        entered.notified().await;
        create.abort();
        release.notify_one();
        assert!(matches!(create.await, Err(error) if error.is_cancelled()));

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if archive.root().open_dir(&id).is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned create transaction did not clean its undelivered record");
    }

    #[tokio::test]
    async fn cancelled_create_cleanup_failure_keeps_charge_and_blocks_spawns() {
        let (_tmp, archive) = root();
        let archive = Arc::new(archive);
        let quota = SessionQuota::from_inventory(
            &scan_inventory(archive.root()).unwrap(),
            SESSION_QUOTA_BYTES,
            archive.clone(),
        );
        let reservation = quota.reserve_for_create().await.reservation.unwrap();
        let (entered, release) = archive.pause_next_create();
        archive.fail_next_discard();
        let create_archive = archive.clone();
        let id = TaskId::new();
        let create_id = id.clone();
        let create = tokio::spawn(async move {
            create_archive
                .create(&create_id, &meta(), reservation)
                .await
        });
        entered.notified().await;
        create.abort();
        release.notify_one();
        assert!(matches!(create.await, Err(error) if error.is_cancelled()));
        archive.settle().await;

        assert_eq!(quota.reserved(), 2 * DEFAULT_STREAM_CAPACITY);
        assert!(archive.root().open_dir(&id).is_ok());
        assert!(matches!(
            quota.reserve_for_create().await.reservation,
            Err(QuotaError::Blocked)
        ));
    }

    #[tokio::test]
    async fn create_rejects_reservation_for_wrong_layout() {
        let (_tmp, archive) = root();
        let archive = Arc::new(archive);
        let quota = SessionQuota::from_inventory(
            &scan_inventory(archive.root()).unwrap(),
            SESSION_QUOTA_BYTES,
            archive.clone(),
        );
        let reservation = quota.reserve_for_test(1).await.reservation.unwrap();
        let id = TaskId::new();
        let error = match archive.create(&id, &meta(), reservation).await {
            Ok(_) => panic!("undersized reservation unexpectedly created a task"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("requires 1048576"));
        assert!(archive.root().open_dir(&id).is_err());
        assert_eq!(quota.reserved(), 0);
    }

    #[tokio::test]
    async fn open_unknown_is_none() {
        let (_tmp, archive) = root();
        let missing = TaskId::new();
        assert!(archive.open(&missing).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn same_id_shares_one_record() {
        let (_tmp, archive) = root();
        let id = TaskId::new();
        let a = archive.create_unreserved(&id, &meta()).await.unwrap();
        let b = archive.open(&id).await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&a, &b), "one live record per id");
    }

    #[tokio::test]
    async fn terminal_status_is_immutable() {
        let (_tmp, archive) = root();
        let id = TaskId::new();
        let record = archive.create_unreserved(&id, &meta()).await.unwrap();
        {
            let mut g = record.lock_commit().await;
            let mut cand = g.current().clone();
            cand.status = TaskStatus::Exited {
                code: Some(0),
                at: jiff::Timestamp::now(),
            };
            g.commit(cand).await.unwrap();
        }
        // A second terminal transition is rejected.
        let mut g = record.lock_commit().await;
        let mut cand = g.current().clone();
        cand.status = TaskStatus::Killed {
            at: jiff::Timestamp::now(),
        };
        assert!(g.commit(cand).await.is_err());
    }

    #[tokio::test]
    async fn cursor_cannot_regress() {
        let (_tmp, archive) = root();
        let id = TaskId::new();
        let record = archive.create_unreserved(&id, &meta()).await.unwrap();
        record.files().stdout.append(b"0123456789").await.unwrap();
        {
            let mut g = record.lock_commit().await;
            let mut cand = g.current().clone();
            cand.stdout_cursor = 5;
            g.commit(cand).await.unwrap();
        }
        let mut g = record.lock_commit().await;
        let mut cand = g.current().clone();
        cand.stdout_cursor = 3;
        assert!(g.commit(cand).await.is_err());
    }

    #[tokio::test]
    async fn consumed_transition_deletes_rings_and_reopens_without_them() {
        let (_tmp, archive) = root();
        let id = TaskId::new();
        let record = archive.create_unreserved(&id, &meta()).await.unwrap();
        record.files().stdout.append(b"abc").await.unwrap();
        record.files().stderr.append(b"de").await.unwrap();
        record.files().flush().await.unwrap();
        // Terminal, fully consumed.
        {
            let mut g = record.lock_commit().await;
            let mut cand = g.current().clone();
            cand.status = TaskStatus::Exited {
                code: Some(0),
                at: jiff::Timestamp::now(),
            };
            cand.stdout_cursor = 3;
            cand.stderr_cursor = 2;
            g.commit(cand).await.unwrap();
            // Now mark Consumed then delete rings (manifest-first).
            let mut cand = g.current().clone();
            cand.disposition = OutputDisposition::Consumed {
                at: jiff::Timestamp::now(),
            };
            g.commit(cand).await.unwrap();
            g.delete_rings().await.unwrap();
        }
        drop(record);

        let reopened = archive.open(&id).await.unwrap().expect("still queryable");
        let g = reopened.lock_commit().await;
        assert!(matches!(
            g.current().disposition,
            OutputDisposition::Consumed { .. }
        ));
        assert!(matches!(g.current().status, TaskStatus::Exited { .. }));
    }
}
