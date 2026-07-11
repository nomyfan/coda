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

use tokio::sync::Mutex;

use super::archive_dir::{ArchiveDir, ArchiveError, ArchiveFileName};
use super::disk_tail::{DiskTail, LAYOUT_VERSION};
use super::manifest::{MANIFEST_VERSION, OutputDisposition, StreamManifest, TaskOutputManifest};
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
    commit: Mutex<TaskPersistentState>,
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
    pub async fn lock_commit(&self) -> TaskCommitGuard<'_> {
        let state = self.commit.lock().await;
        TaskCommitGuard {
            record: self,
            state,
        }
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
pub struct TaskCommitGuard<'a> {
    record: &'a TaskRecord,
    state: tokio::sync::MutexGuard<'a, TaskPersistentState>,
}

impl TaskCommitGuard<'_> {
    pub fn current(&self) -> &TaskPersistentState {
        &self.state
    }

    pub fn record(&self) -> &TaskRecord {
        self.record
    }

    /// Validate `current → candidate` against the monotonic invariants, save
    /// the manifest atomically, and only then replace the in-memory state. On
    /// any error the memory state is untouched.
    pub async fn commit(&mut self, candidate: TaskPersistentState) -> Result<(), ArchiveError> {
        check_transition(&self.state, &candidate)?;
        let manifest = self.record.build_manifest(&candidate).await;
        validate_cursor_bounds(&manifest)?;
        let task_dir = self.record.task_dir.clone();
        tokio::task::spawn_blocking(move || save_manifest(&task_dir, &manifest))
            .await
            .map_err(join_err)??;
        *self.state = candidate;
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
pub struct TaskArchive {
    root: ArchiveDir,
    index: StdMutex<HashMap<TaskId, Weak<TaskRecord>>>,
}

impl TaskArchive {
    pub fn new(root: ArchiveDir) -> Self {
        TaskArchive {
            root,
            index: StdMutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &ArchiveDir {
        &self.root
    }

    /// Create a task's private `0700` directory and two `0600` ring files, and
    /// register the live record. All-or-nothing: on any failure the partial
    /// directory is removed and no record is returned.
    pub async fn create(
        &self,
        id: &TaskId,
        meta: &TaskMeta,
    ) -> Result<Arc<TaskRecord>, ArchiveError> {
        let root = self.root.clone();
        let id_owned = id.clone();
        let (task_dir, stdout_file, stderr_file) = tokio::task::spawn_blocking(move || {
            let task_dir = root.create_dir(&id_owned)?;
            let build = (|| {
                let out = task_dir.create_file(ArchiveFileName::StdoutRing)?;
                let err = task_dir.create_file(ArchiveFileName::StderrRing)?;
                Ok::<_, ArchiveError>((out, err))
            })();
            match build {
                Ok((out, err)) => Ok((task_dir, out, err)),
                Err(e) => {
                    // Roll back the half-built directory we just created.
                    let _ = task_dir.unlink(ArchiveFileName::StdoutRing);
                    let _ = task_dir.unlink(ArchiveFileName::StderrRing);
                    let _ = root.remove_dir(&id_owned);
                    Err(e)
                }
            }
        })
        .await
        .map_err(join_err)??;

        let stdout = Arc::new(DiskTail::create(stdout_file, DEFAULT_STREAM_CAPACITY)?);
        let stderr = Arc::new(DiskTail::create(stderr_file, DEFAULT_STREAM_CAPACITY)?);
        let state = TaskPersistentState {
            status: TaskStatus::Running,
            stdout_cursor: 0,
            stderr_cursor: 0,
            stdout_carry: Vec::new(),
            stderr_carry: Vec::new(),
            disposition: OutputDisposition::Retained,
        };
        let record = Arc::new(TaskRecord {
            id: id.clone(),
            meta: meta.clone(),
            started_at: jiff::Timestamp::now(),
            capacities: (DEFAULT_STREAM_CAPACITY, DEFAULT_STREAM_CAPACITY),
            task_dir,
            files: TaskOutputFiles { stdout, stderr },
            commit: Mutex::new(state),
        });

        // Persist the initial Running manifest before handing out the record.
        {
            let mut guard = record.lock_commit().await;
            let candidate = guard.current().clone();
            guard.commit(candidate).await?;
        }

        self.index
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::downgrade(&record));
        Ok(record)
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
            commit: Mutex::new(state),
        }))
    }
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
        let record = archive.create(&id, &meta()).await.unwrap();
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
    async fn open_unknown_is_none() {
        let (_tmp, archive) = root();
        let missing = TaskId::new();
        assert!(archive.open(&missing).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn same_id_shares_one_record() {
        let (_tmp, archive) = root();
        let id = TaskId::new();
        let a = archive.create(&id, &meta()).await.unwrap();
        let b = archive.open(&id).await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&a, &b), "one live record per id");
    }

    #[tokio::test]
    async fn terminal_status_is_immutable() {
        let (_tmp, archive) = root();
        let id = TaskId::new();
        let record = archive.create(&id, &meta()).await.unwrap();
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
        let record = archive.create(&id, &meta()).await.unwrap();
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
        let record = archive.create(&id, &meta()).await.unwrap();
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
