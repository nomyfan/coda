//! Task lifecycle commits remain independent of output retention.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex as StdMutex, Weak};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, watch};

use super::archive_dir::{ArchiveDir, ArchiveError, ArchiveFileName};
use super::manifest::{MANIFEST_VERSION, TaskOutputManifest};
use super::{TaskMeta, TaskStatus};
pub use crate::output::TaskOutputFiles;
use coda_core::output::{CapturePurpose, Channel, OutputOwner, OutputStore};
use coda_core::task::TaskId;

/// Largest `meta.json` we will read; a larger file is treated as corrupt
/// without materialising its contents.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// The mutable, persisted slice of a task's state — everything the commit lock
/// serializes. Ring `start_offset`/`total_written` are owned by `DiskTail` and
/// snapshotted at commit time, so they are not duplicated here.
#[derive(Debug, Clone)]
pub struct TaskPersistentState {
    pub notice: Option<super::manifest::NoticeDelivery>,
    pub cleanup_pending: bool,
    pub scope_members: Vec<coda_core::task::ScopeMember>,
    pub status: TaskStatus,
    /// The runtime terminal state is newer than the durable manifest. Set only
    /// by the final in-memory Failed degradation and cleared by any successful
    /// manifest commit.
    pub persistence_dirty: bool,
}

/// One archived task: immutable identity plus the guarded mutable state and the
/// shared output handles. The same `TaskId` resolves to the same live record (and thus
/// the same commit lock) process-wide via [`TaskArchive`]'s weak index.
pub struct TaskRecord {
    id: TaskId,
    meta: TaskMeta,
    started_at: jiff::Timestamp,
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

    pub async fn write_result(&self, answer: &str) -> Result<(), ArchiveError> {
        self.files
            .result
            .append(answer.as_bytes())
            .await
            .map_err(Into::into)
    }

    /// Snapshot lifecycle and output state after any required finalization.
    async fn build_manifest(&self, state: &TaskPersistentState) -> TaskOutputManifest {
        TaskOutputManifest {
            payload: self.files.snapshot(),
            notice: state.notice.clone(),
            cleanup_pending: state.cleanup_pending,
            scope_members: state.scope_members.clone(),
            manifest_version: MANIFEST_VERSION,
            id: self.id.clone(),
            meta: self.meta.clone(),
            started_at: self.started_at,
            terminal_at: state.status.terminal_at(),
            status: state.status.clone(),
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
        if self.record.meta().is_subagent() {
            candidate.notice = Some(super::manifest::NoticeDelivery::Pending);
        }
        candidate.persistence_dirty = true;
        check_transition(self.current(), &candidate)?;
        *self.state.as_deref_mut().expect("commit guard owns state") = candidate;
        Ok(())
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
    Ok(())
}

/// The lazy index mapping a `TaskId` to its single live record, so concurrent
/// opens of the same id share one commit lock. Inventory never inserts here;
/// only an active create or an explicit `open(id)` materialises a record.
#[derive(Clone)]
pub struct TaskArchive {
    output_store: Arc<coda_output::Store>,
    output_owner: OutputOwner,
    root: ArchiveDir,
    index: Arc<StdMutex<HashMap<TaskId, Weak<TaskRecord>>>>,
    activity: Arc<ArchiveActivity>,
    #[cfg(test)]
    test_hooks: Arc<TaskArchiveTestHooks>,
}

/// Shared deterministic scheduling and fault-injection state for archive tests.
#[cfg(test)]
#[derive(Default)]
struct TaskArchiveTestHooks {
    fail_next_initial_manifest: AtomicBool,
    create_pause: StdMutex<Option<CreatePause>>,
    fail_next_discard: AtomicBool,
}

impl TaskArchive {
    pub fn new(root: ArchiveDir) -> Self {
        Self::with_output(
            root,
            coda_output::Store::standalone(),
            OutputOwner {
                workspace_id: String::new(),
                session_id: String::new(),
            },
        )
    }

    pub fn with_output(
        root: ArchiveDir,
        output_store: Arc<coda_output::Store>,
        output_owner: OutputOwner,
    ) -> Self {
        TaskArchive {
            output_store,
            output_owner,
            root,
            index: Arc::new(StdMutex::new(HashMap::new())),
            activity: Arc::new(ArchiveActivity {
                count: watch::channel(0).0,
            }),
            #[cfg(test)]
            test_hooks: Arc::new(TaskArchiveTestHooks::default()),
        }
    }

    pub fn root(&self) -> &ArchiveDir {
        &self.root
    }

    pub(crate) async fn create(
        &self,
        id: &TaskId,
        meta: &TaskMeta,
    ) -> Result<Arc<TaskRecord>, ArchiveError> {
        self.create_transaction(id, meta).await
    }

    async fn create_transaction(
        &self,
        id: &TaskId,
        meta: &TaskMeta,
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
            let acknowledged = result_tx.send(result).is_ok() && ack_rx.await.is_ok();
            if !acknowledged
                && let Some(record) = created
                && let Err(error) = archive.discard_created(&record).await
            {
                tracing::error!(%error, task = %id, "detached task create cleanup failed");
            }
        });
        let result = result_rx
            .await
            .map_err(|_| ArchiveError::corrupt("task create stopped"))?;
        let _ = ack_tx.send(());
        result.map_err(|failure| failure.error)
    }

    async fn create_inner(
        &self,
        id: &TaskId,
        meta: &TaskMeta,
    ) -> Result<Arc<TaskRecord>, CreateFailure> {
        #[cfg(test)]
        let pause = { self.test_hooks.create_pause.lock().unwrap().take() };
        #[cfg(test)]
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
        let root = self.root.clone();
        let id_owned = id.clone();
        let task_dir = tokio::task::spawn_blocking(move || root.create_dir(&id_owned))
            .await
            .map_err(|error| CreateFailure::new(join_err(error)))?
            .map_err(CreateFailure::new)?;
        let capture = match self
            .output_store
            .begin(
                self.output_owner.clone(),
                vec![Channel::Stdout, Channel::Stderr, Channel::Result],
                CapturePurpose::Background,
            )
            .await
        {
            Ok(capture) => capture,
            Err(error) => {
                return Err(CreateFailure::after_cleanup(
                    ArchiveError::corrupt(error.to_string()),
                    rollback_created_task(self.root.clone(), id.clone(), task_dir).await,
                ));
            }
        };
        let files = TaskOutputFiles::capturing(capture);
        let state = TaskPersistentState {
            notice: None,
            cleanup_pending: false,
            scope_members: vec![],
            status: TaskStatus::Running,
            persistence_dirty: false,
        };
        let record = Arc::new(TaskRecord {
            id: id.clone(),
            meta: meta.clone(),
            started_at: jiff::Timestamp::now(),
            task_dir,
            files,
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
                .test_hooks
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
    /// starting its process failed.
    pub async fn discard_created(&self, record: &TaskRecord) -> Result<(), ArchiveError> {
        self.index.lock().unwrap().remove(record.id());
        #[cfg(test)]
        if self
            .test_hooks
            .fail_next_discard
            .swap(false, Ordering::SeqCst)
        {
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

    /// Reconstruct lifecycle state and a retained output reader.
    async fn reopen_record(
        &self,
        id: &TaskId,
        task_dir: ArchiveDir,
        manifest: TaskOutputManifest,
    ) -> Result<Arc<TaskRecord>, ArchiveError> {
        let mut snapshot = manifest.payload.clone();
        if !snapshot.sealed {
            snapshot.sealed = true;
            snapshot.failure = Some(coda_core::output::StorageFailure::Incomplete);
            snapshot.reference = None;
        }
        let files = TaskOutputFiles::retained(self.output_store.reader(snapshot));
        let state = TaskPersistentState {
            notice: manifest.notice.clone(),
            cleanup_pending: manifest.cleanup_pending,
            scope_members: manifest.scope_members.clone(),
            status: manifest.status.clone(),
            persistence_dirty: false,
        };
        Ok(Arc::new(TaskRecord {
            id: id.clone(),
            meta: manifest.meta,
            started_at: manifest.started_at,
            task_dir,
            files,
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
}

impl CreateFailure {
    fn new(error: ArchiveError) -> Self {
        Self { error }
    }

    fn after_cleanup(error: ArchiveError, cleanup: Result<(), ArchiveError>) -> Self {
        match cleanup {
            Ok(()) => Self::new(error),
            Err(cleanup) => Self {
                error: ArchiveError::corrupt(format!(
                    "{error}; task create rollback failed: {cleanup}"
                )),
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
    for name in [ArchiveFileName::MetaTmp, ArchiveFileName::Meta] {
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
pub(crate) fn load_task_dir(
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
    let file = task_dir.open_file(ArchiveFileName::Meta, false)?;
    let len = file.metadata()?.len();
    if len > MAX_MANIFEST_BYTES {
        return Err(ArchiveError::corrupt(format!(
            "meta.json is {len} bytes, over the {MAX_MANIFEST_BYTES} cap"
        )));
    }
    let mut buf = Vec::with_capacity(len as usize);
    file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ArchiveError::corrupt(
            "meta.json grew beyond its size limit",
        ));
    }
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
    let terminal = !manifest.status.is_running();
    if terminal != manifest.terminal_at.is_some() {
        return Err(ArchiveError::corrupt(
            "terminal_at presence disagrees with status",
        ));
    }
    Ok(())
}

/// Atomically persist a manifest: temp write + fsync + rename (blocking).
fn save_manifest(task_dir: &ArchiveDir, manifest: &TaskOutputManifest) -> Result<(), ArchiveError> {
    let json = serde_json::to_vec_pretty(manifest)
        .map_err(|e| ArchiveError::corrupt(format!("manifest serialize error: {e}")))?;
    if json.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ArchiveError::corrupt(format!(
            "meta.json is {} bytes, over the {MAX_MANIFEST_BYTES} cap",
            json.len()
        )));
    }
    // Clear any crash-leftover temp before O_EXCL create.
    task_dir.unlink(ArchiveFileName::MetaTmp)?;
    let mut file = task_dir.create_file(ArchiveFileName::MetaTmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    task_dir.rename(ArchiveFileName::MetaTmp, ArchiveFileName::Meta)?;
    task_dir.sync()?;
    Ok(())
}

fn join_err(err: tokio::task::JoinError) -> ArchiveError {
    ArchiveError::corrupt(format!("archive worker failed: {err}"))
}

#[cfg(test)]
#[path = "task_archive_tests.rs"]
mod tests;
