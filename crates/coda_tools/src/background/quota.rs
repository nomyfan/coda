//! Session-local archive inventory and the 64 MiB payload quota.
//!
//! **Inventory** (`scan_inventory`) is entry-reopen recovery, not startup
//! cleanup: it streams the `background/tasks` children, classifies and *charges*
//! each without deleting or repairing anything, and produces only bounded,
//! fd-free outputs — a ≤512 retained victim index, ≤32 recent summaries, ≤32
//! issue samples, and totals. Any corrupt/orphan/unsafe entry sets a session
//! spawn blocker so unexplained bytes can never bypass the quota; measurable
//! bytes are charged conservatively.
//!
//! **`SessionQuota`** reserves capacity for new tasks and evicts the oldest
//! terminal-Retained victims when a create needs room. One async transaction
//! gate spans residual cleanup, victim manifest commit, ring deletion, and
//! accounting release. Its global order is quota transaction ⟶ per-task commit
//! ⟶ stream. A small leaf `std::sync::Mutex` holds counters/indexes so an
//! unpublished create reservation can still roll back synchronously in Drop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, watch};

use super::archive_dir::{ArchiveDir, ArchiveError, ArchiveFileName, EntryKind};
use super::manifest::{OutputDisposition, StreamManifest, TaskOutputManifest};
use super::task_archive::{TaskArchive, TaskRecord, read_manifest, validate_manifest};
use super::task_id::TaskId;
use super::{TaskStatus, TaskSummary};

/// Session ring payload quota: reserved as the sum of both streams' manifest
/// capacities per task. `64 MiB >= MAX_RUNNING(16) × 2 × 512 KiB`, so every
/// active task is guaranteed a reservation.
pub const SESSION_QUOTA_BYTES: u64 = 64 * 1024 * 1024;

const MAX_RETAINED_INDEX: usize = 512;
const MAX_RECOVERABLE_RUNNING: usize = 512;
const MAX_RESIDUAL_INDEX: usize = 512;
const MAX_RECENT_TERMINAL: usize = 32;
const MAX_ISSUE_SAMPLES: usize = 32;
const ISSUE_TEXT_LIMIT: usize = 512;

/// Bounded, fd-free result of an inventory scan.
#[derive(Debug, Default)]
pub struct ArchiveInventory {
    /// Retained terminal victims (id + terminal_at + capacities); no fd/record.
    pub retained: Vec<RetainedIndexEntry>,
    pub retained_count: u64,
    pub retained_index_truncated: bool,
    /// Newest ≤32 terminal summaries for the live overview.
    pub recent_terminal: Vec<TaskSummary>,
    pub reserved_bytes: u64,
    pub issue_count: u64,
    pub sampled_issues: Vec<InventoryIssue>,
    pub spawn_blocked: bool,
    /// Valid crash-`Running` tasks whose rings pass length validation — eligible
    /// for `Interrupted` conversion at reopen (phase: hub lifecycle).
    pub recoverable_running: Vec<TaskId>,
    /// Consumed/Expired tasks whose prior ring deletion did not finish. They
    /// remain charged and are retried by the next create, never during attach.
    pub(crate) residual_deletes: Vec<ResidualIndexEntry>,
    pub(crate) residual_index_truncated: bool,
}

/// One evictable terminal-Retained task. No fd, record, or commit lock.
#[derive(Debug, Clone)]
pub struct RetainedIndexEntry {
    pub id: TaskId,
    pub terminal_at: jiff::Timestamp,
    pub stdout_capacity: u64,
    pub stderr_capacity: u64,
}

impl RetainedIndexEntry {
    fn reserved(&self) -> u64 {
        self.stdout_capacity + self.stderr_capacity
    }
}

/// A durable non-readable disposition with ring files still present.
#[derive(Debug, Clone)]
pub struct ResidualIndexEntry {
    pub id: TaskId,
    pub reserved_bytes: u64,
}

#[derive(Debug, Clone)]
pub enum InventoryIssue {
    CorruptTask {
        id: Option<TaskId>,
        charged_bytes: u64,
        error: String,
    },
    OrphanEntry {
        name: String,
        charged_bytes: u64,
    },
    UnsafeEntry {
        name: String,
        error: String,
    },
}

/// Stream the archive root and classify every task directory. Blocking; runs on
/// the blocking pool. Opens at most a constant number of fds at once (the dir
/// iterator plus one task dir being examined).
pub fn scan_inventory(root: &ArchiveDir) -> Result<ArchiveInventory, ArchiveError> {
    let mut inv = ArchiveInventory::default();
    let entries = root.entries()?;
    for entry in entries {
        let entry = entry?;
        classify_entry(root, &entry.name, entry.kind, &mut inv);
    }
    compact_recent(&mut inv.recent_terminal);
    inv.spawn_blocked = inv.issue_count > 0
        || inv.retained_index_truncated
        || inv.residual_index_truncated
        || inv.reserved_bytes > SESSION_QUOTA_BYTES;
    Ok(inv)
}

fn classify_entry(root: &ArchiveDir, name: &str, kind: EntryKind, inv: &mut ArchiveInventory) {
    // A non-canonical or symlinked name can never be one of our task dirs, and
    // the typed API only descends by TaskId — treat it as an unsafe orphan.
    let Ok(id) = name.parse::<TaskId>() else {
        record_issue(
            inv,
            InventoryIssue::UnsafeEntry {
                name: truncate(name),
                error: "not a canonical task directory name".into(),
            },
        );
        return;
    };
    if matches!(kind, EntryKind::Symlink | EntryKind::Other) {
        record_issue(
            inv,
            InventoryIssue::UnsafeEntry {
                name: truncate(name),
                error: format!("unexpected entry kind {kind:?}"),
            },
        );
        return;
    }

    let task_dir = match root.open_dir(&id) {
        Ok(d) => d,
        Err(e) => {
            record_issue(
                inv,
                InventoryIssue::UnsafeEntry {
                    name: truncate(name),
                    error: e.to_string(),
                },
            );
            return;
        }
    };

    let manifest = match read_manifest(&task_dir) {
        Ok(m) => m,
        Err(ArchiveError::Io(e)) if e.raw_os_error() == Some(libc::ENOENT) => {
            // Rings but no manifest: an orphan.
            let charged = charge_ring_files(&task_dir);
            record_issue(
                inv,
                InventoryIssue::OrphanEntry {
                    name: truncate(name),
                    charged_bytes: charged,
                },
            );
            inv.reserved_bytes += charged;
            return;
        }
        Err(e) => {
            let charged = charge_ring_files(&task_dir);
            record_issue(
                inv,
                InventoryIssue::CorruptTask {
                    id: Some(id),
                    charged_bytes: charged,
                    error: truncate(&e.to_string()),
                },
            );
            inv.reserved_bytes += charged;
            return;
        }
    };

    if let Err(e) = validate_manifest(&id, &manifest) {
        let charged = charge_ring_files(&task_dir);
        record_issue(
            inv,
            InventoryIssue::CorruptTask {
                id: Some(id),
                charged_bytes: charged,
                error: truncate(&e.to_string()),
            },
        );
        inv.reserved_bytes += charged;
        return;
    }

    classify_valid(&task_dir, id, manifest, inv);
}

fn classify_valid(
    task_dir: &ArchiveDir,
    id: TaskId,
    manifest: TaskOutputManifest,
    inv: &mut ArchiveInventory,
) {
    match &manifest.status {
        TaskStatus::Running => {
            // Crash leftover. Recoverable only if both ring lengths match the
            // manifest; otherwise it is a corrupt task, charged and blocking.
            if rings_length_ok(task_dir, &manifest) {
                inv.reserved_bytes += manifest.stdout.capacity + manifest.stderr.capacity;
                if inv.recoverable_running.len() < MAX_RECOVERABLE_RUNNING {
                    inv.recoverable_running.push(id);
                } else {
                    record_issue(
                        inv,
                        InventoryIssue::CorruptTask {
                            id: Some(id),
                            charged_bytes: manifest.stdout.capacity + manifest.stderr.capacity,
                            error: "recoverable Running task limit exceeded".into(),
                        },
                    );
                }
            } else {
                let charged = charge_ring_files(task_dir);
                record_issue(
                    inv,
                    InventoryIssue::CorruptTask {
                        id: Some(id),
                        charged_bytes: charged,
                        error: "Running ring length disagrees with manifest".into(),
                    },
                );
                inv.reserved_bytes += charged;
            }
        }
        _terminal => {
            note_recent_terminal(inv, summary_of(&id, &manifest));
            match &manifest.output {
                OutputDisposition::Retained => {
                    if rings_length_ok(task_dir, &manifest) {
                        inv.reserved_bytes += manifest.stdout.capacity + manifest.stderr.capacity;
                        inv.retained_count += 1;
                        if inv.retained.len() < MAX_RETAINED_INDEX {
                            inv.retained.push(RetainedIndexEntry {
                                id,
                                terminal_at: manifest.terminal_at.unwrap_or(manifest.started_at),
                                stdout_capacity: manifest.stdout.capacity,
                                stderr_capacity: manifest.stderr.capacity,
                            });
                        } else {
                            inv.retained_index_truncated = true;
                        }
                    } else {
                        let charged = charge_ring_files(task_dir);
                        record_issue(
                            inv,
                            InventoryIssue::CorruptTask {
                                id: Some(id),
                                charged_bytes: charged,
                                error: "Retained ring length disagrees with manifest".into(),
                            },
                        );
                        inv.reserved_bytes += charged;
                    }
                }
                OutputDisposition::Consumed { .. } | OutputDisposition::Expired { .. } => {
                    match residual_ring_bytes(task_dir) {
                        Ok(Some(reserved_bytes)) => {
                            inv.reserved_bytes += reserved_bytes;
                            if inv.residual_deletes.len() < MAX_RESIDUAL_INDEX {
                                inv.residual_deletes
                                    .push(ResidualIndexEntry { id, reserved_bytes });
                            } else {
                                inv.residual_index_truncated = true;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let charged = charge_ring_files(task_dir);
                            record_issue(
                                inv,
                                InventoryIssue::CorruptTask {
                                    id: Some(id),
                                    charged_bytes: charged,
                                    error: truncate(&e.to_string()),
                                },
                            );
                            inv.reserved_bytes += charged;
                        }
                    }
                }
            }
        }
    }
}

fn summary_of(id: &TaskId, manifest: &TaskOutputManifest) -> TaskSummary {
    TaskSummary {
        id: id.as_str().to_owned(),
        command: manifest.command.clone(),
        description: manifest.description.clone(),
        agent_name: manifest.agent_name.clone(),
        status: manifest.status.clone(),
        started_at: manifest.started_at,
    }
}

/// Keep the newest `MAX_RECENT_TERMINAL` by terminal time with bounded memory:
/// let the buffer grow to 2× then compact.
fn note_recent_terminal(inv: &mut ArchiveInventory, summary: TaskSummary) {
    inv.recent_terminal.push(summary);
    if inv.recent_terminal.len() > MAX_RECENT_TERMINAL * 2 {
        compact_recent(&mut inv.recent_terminal);
    }
}

fn compact_recent(v: &mut Vec<TaskSummary>) {
    v.sort_by(|a, b| {
        let at = |s: &TaskSummary| s.status.terminal_at().unwrap_or(s.started_at);
        at(b).cmp(&at(a))
    });
    v.truncate(MAX_RECENT_TERMINAL);
}

fn record_issue(inv: &mut ArchiveInventory, issue: InventoryIssue) {
    inv.issue_count += 1;
    if inv.sampled_issues.len() < MAX_ISSUE_SAMPLES {
        inv.sampled_issues.push(issue);
    }
}

/// Sum the lengths of the task dir's ring files that open safely as regular
/// files; unopenable/irregular ones contribute nothing (but the caller has
/// already flagged the task as an issue).
fn charge_ring_files(task_dir: &ArchiveDir) -> u64 {
    let mut total = 0;
    for name in [ArchiveFileName::StdoutRing, ArchiveFileName::StderrRing] {
        if let Ok(file) = task_dir.open_file(name, false)
            && let Ok(meta) = file.metadata()
        {
            total += meta.len();
        }
    }
    total
}

/// Return the measurable bytes of safely-opened leftover rings, or `None` when
/// both are absent. Any present unsafe ring is corruption, not a residual.
fn residual_ring_bytes(task_dir: &ArchiveDir) -> Result<Option<u64>, ArchiveError> {
    let mut total = 0;
    let mut present = false;
    for name in [ArchiveFileName::StdoutRing, ArchiveFileName::StderrRing] {
        match task_dir.open_file(name, false) {
            Ok(file) => {
                present = true;
                total += file.metadata()?.len();
            }
            Err(ArchiveError::Io(e)) if e.raw_os_error() == Some(libc::ENOENT) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(present.then_some(total))
}

/// A Running task's rings are consistent iff each file length equals
/// `min(total_written, capacity)`.
fn rings_length_ok(task_dir: &ArchiveDir, manifest: &TaskOutputManifest) -> bool {
    let ok = |name: ArchiveFileName, m: &StreamManifest| -> bool {
        task_dir
            .open_file(name, false)
            .and_then(|f| f.metadata().map_err(Into::into))
            .map(|meta| meta.len() == m.total_written.min(m.capacity))
            .unwrap_or(false)
    };
    ok(ArchiveFileName::StdoutRing, &manifest.stdout)
        && ok(ArchiveFileName::StderrRing, &manifest.stderr)
}

fn truncate(s: &str) -> String {
    if s.len() <= ISSUE_TEXT_LIMIT {
        s.to_owned()
    } else {
        // Truncate on a char boundary at or below the limit.
        let mut end = ISSUE_TEXT_LIMIT;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_owned()
    }
}

// ---- SessionQuota ---------------------------------------------------------

/// Why a `reserve_for_create` failed.
#[derive(Debug)]
pub enum QuotaError {
    /// A prior inventory found corrupt/orphan/over-quota state; no new spawn.
    Blocked,
    /// Even after evicting every terminal victim the reservation would exceed
    /// the quota (should not happen under the active-worst-case invariant).
    Exhausted,
    /// A reservation capability was minted for a different task layout.
    LayoutMismatch {
        reserved: u64,
        required: u64,
    },
    Archive(ArchiveError),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Blocked => f.write_str(
                "background archive inventory blocked new spawns (corrupt or over-quota state)",
            ),
            QuotaError::Exhausted => f.write_str("session output quota exhausted"),
            QuotaError::LayoutMismatch { reserved, required } => write!(
                f,
                "quota reservation is {reserved} bytes, but task layout requires {required}"
            ),
            QuotaError::Archive(e) => write!(f, "quota archive error: {e}"),
        }
    }
}

impl std::error::Error for QuotaError {}

impl From<ArchiveError> for QuotaError {
    fn from(e: ArchiveError) -> Self {
        QuotaError::Archive(e)
    }
}

/// A newly-recorded expiration to be delivered as an `OutputExpired` notice by
/// the registry (the quota layer never touches the notice queue).
#[derive(Debug, Clone)]
pub struct ExpirationFact {
    pub id: TaskId,
    pub expired_at: jiff::Timestamp,
    pub reason: super::manifest::ExpireReason,
}

/// One create attempt. Expirations are returned even when reservation failed:
/// their manifests may already be durable and the registry must publish them.
pub struct ReserveOutcome {
    pub reservation: Result<QuotaReservation, QuotaError>,
    pub expirations: Vec<ExpirationFact>,
}

struct QuotaInner {
    reserved: u64,
    retained: Vec<RetainedIndexEntry>,
    residual_deletes: Vec<ResidualIndexEntry>,
    pending_expirations: Vec<ExpirationFact>,
    spawn_blocked: bool,
}

/// Serializes create/eviction/release for one session's ring payload. The async
/// gate is the transaction boundary; the leaf mutex exists so reservation Drop
/// can synchronously roll back a not-yet-published create.
#[derive(Clone)]
pub struct SessionQuota {
    limit: u64,
    transaction: Arc<AsyncMutex<()>>,
    inner: Arc<Mutex<QuotaInner>>,
    archive: Arc<TaskArchive>,
    activity: Arc<QuotaActivity>,
    #[cfg(test)]
    fail_next_delete: Arc<AtomicBool>,
    #[cfg(test)]
    delete_pause: Arc<Mutex<Option<DeletePause>>>,
}

impl SessionQuota {
    /// Initialise from an inventory: existing reservations, victim index, and
    /// blocker. No deletion or eviction happens here.
    pub fn from_inventory(
        inventory: &ArchiveInventory,
        limit: u64,
        archive: Arc<TaskArchive>,
    ) -> Self {
        SessionQuota {
            limit,
            transaction: Arc::new(AsyncMutex::new(())),
            inner: Arc::new(Mutex::new(QuotaInner {
                reserved: inventory.reserved_bytes,
                retained: inventory.retained.clone(),
                residual_deletes: inventory.residual_deletes.clone(),
                pending_expirations: Vec::new(),
                spawn_blocked: inventory.spawn_blocked,
            })),
            archive,
            activity: Arc::new(QuotaActivity {
                count: watch::channel(0).0,
            }),
            #[cfg(test)]
            fail_next_delete: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            delete_pause: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn reserved(&self) -> u64 {
        self.inner.lock().unwrap().reserved
    }

    #[cfg(test)]
    pub(crate) fn retained_contains(&self, id: &TaskId) -> bool {
        self.inner
            .lock()
            .unwrap()
            .retained
            .iter()
            .any(|entry| entry.id == *id)
    }

    #[cfg(test)]
    fn fail_next_delete(&self) {
        self.fail_next_delete.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn pause_next_delete(&self) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *self.delete_pause.lock().unwrap() = Some(DeletePause {
            entered: entered.clone(),
            release: release.clone(),
        });
        (entered, release)
    }

    /// Reserve `bytes` for a new task. Residual deletes are retried first; then
    /// terminal victims are evicted oldest-first. A victim remains charged
    /// until both ring deletes succeed. Facts survive every error return.
    pub async fn reserve_for_create(&self) -> ReserveOutcome {
        self.reserve_bytes(2 * super::task_archive::DEFAULT_STREAM_CAPACITY)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn reserve_for_test(&self, bytes: u64) -> ReserveOutcome {
        self.reserve_bytes(bytes).await
    }

    async fn reserve_bytes(&self, bytes: u64) -> ReserveOutcome {
        let quota = self.clone();
        let activity = self.begin_activity();
        let transaction = tokio::spawn(async move {
            let _activity = activity;
            let _transaction = quota.transaction.lock().await;
            quota.reserve_locked(bytes).await
        });
        let reservation = match transaction.await {
            Ok(reservation) => reservation,
            Err(error) => Err(QuotaError::Archive(ArchiveError::corrupt(format!(
                "quota transaction stopped: {error}"
            )))),
        };
        let expirations = self.take_expirations();
        ReserveOutcome {
            reservation,
            expirations,
        }
    }

    async fn reserve_locked(&self, bytes: u64) -> Result<QuotaReservation, QuotaError> {
        if self.inner.lock().unwrap().spawn_blocked {
            return Err(QuotaError::Blocked);
        }
        self.retry_residual_deletes().await?;

        while self.inner.lock().unwrap().reserved + bytes > self.limit {
            let victim = {
                let mut inner = self.inner.lock().unwrap();
                let Some(idx) = oldest_victim(&inner.retained) else {
                    return Err(QuotaError::Exhausted);
                };
                // Claim only the index entry. Its reservation stays charged
                // until manifest and both deletes have completed.
                inner.retained.swap_remove(idx)
            };
            let mut claim = VictimClaim::new(self.inner.clone(), victim);
            self.evict_victim(&mut claim).await?;
        }

        self.inner.lock().unwrap().reserved += bytes;
        Ok(QuotaReservation {
            lease: Arc::new(ReservationLease {
                inner: self.inner.clone(),
                bytes,
                committed: AtomicBool::new(false),
            }),
        })
    }

    /// Evict one claimed victim: reopen it, recheck under its commit lock, and
    /// commit Consumed (if fully read) or Expired, then delete the rings. A
    /// fully-consumed victim yields no expiration fact.
    async fn evict_victim(&self, claim: &mut VictimClaim) -> Result<(), QuotaError> {
        let victim_id = claim.victim().id.clone();
        let Some(record) = self.archive.open(&victim_id).await? else {
            return Err(QuotaError::Archive(ArchiveError::corrupt(
                "quota victim disappeared from the archive",
            )));
        };
        let mut guard = record.lock_commit().await;
        let current = guard.current().clone();
        // Only a still-Retained terminal is evictable.
        if current.disposition != OutputDisposition::Retained || current.status.is_running() {
            return Err(QuotaError::Archive(ArchiveError::corrupt(
                "quota victim is no longer terminal Retained",
            )));
        }
        let now = jiff::Timestamp::now();
        let fully_consumed = current.stdout_cursor == record.files().stdout.logical_range().await.1
            && current.stderr_cursor == record.files().stderr.logical_range().await.1
            && current.stdout_carry.is_empty()
            && current.stderr_carry.is_empty();

        let mut candidate = current;
        if fully_consumed {
            candidate.disposition = OutputDisposition::Consumed { at: now };
            guard.commit(candidate).await?;
            claim.mark_nonreadable(None);
        } else {
            let reason = super::manifest::ExpireReason::SessionQuota;
            candidate.disposition = OutputDisposition::Expired { at: now, reason };
            guard.commit(candidate).await?;
            let fact = ExpirationFact {
                id: victim_id,
                expired_at: now,
                reason,
            };
            claim.mark_nonreadable(Some(fact));
        }
        self.delete_rings(&guard).await?;
        claim.release();
        Ok(())
    }

    async fn retry_residual_deletes(&self) -> Result<(), QuotaError> {
        loop {
            let residual = self.inner.lock().unwrap().residual_deletes.first().cloned();
            let Some(residual) = residual else {
                return Ok(());
            };
            let Some(record) = self.archive.open(&residual.id).await? else {
                return Err(QuotaError::Archive(ArchiveError::corrupt(
                    "quota residual disappeared from the archive",
                )));
            };
            let guard = record.lock_commit().await;
            if guard.current().disposition.rings_present() {
                return Err(QuotaError::Archive(ArchiveError::corrupt(
                    "quota residual became readable again",
                )));
            }
            self.delete_rings(&guard).await?;
            drop(guard);
            let mut inner = self.inner.lock().unwrap();
            if let Some(index) = inner
                .residual_deletes
                .iter()
                .position(|entry| entry.id == residual.id)
            {
                let removed = inner.residual_deletes.swap_remove(index);
                inner.reserved = inner.reserved.saturating_sub(removed.reserved_bytes);
            }
        }
    }

    /// After a task's terminal manifest is committed (commit lock released),
    /// recheck: fully consumed → Consumed cleanup + release; otherwise register
    /// it as an evictable victim (its reservation stays until evicted/consumed).
    pub async fn finalize_terminal(&self, record: &Arc<TaskRecord>) -> Result<(), ArchiveError> {
        self.finalize_owned(record.clone(), false).await
    }

    /// After a cursor commit, release a terminal task that just became fully
    /// consumed. A no-op while Running or not yet fully read.
    pub async fn finalize_consumed(&self, record: &Arc<TaskRecord>) -> Result<(), ArchiveError> {
        self.finalize_owned(record.clone(), true).await
    }

    async fn finalize_owned(
        &self,
        record: Arc<TaskRecord>,
        consumed_only: bool,
    ) -> Result<(), ArchiveError> {
        let quota = self.clone();
        let activity = self.begin_activity();
        tokio::spawn(async move {
            let _activity = activity;
            let _transaction = quota.transaction.lock().await;
            quota.finalize(&record, consumed_only).await
        })
        .await
        .map_err(|error| ArchiveError::corrupt(format!("quota finalize stopped: {error}")))?
    }

    async fn finalize(
        &self,
        record: &Arc<TaskRecord>,
        consumed_only: bool,
    ) -> Result<(), ArchiveError> {
        let mut guard = record.lock_commit().await;
        let current = guard.current().clone();
        if current.status.is_running() || current.disposition != OutputDisposition::Retained {
            return Ok(());
        }
        let stdout_total = record.files().stdout.logical_range().await.1;
        let stderr_total = record.files().stderr.logical_range().await.1;
        let fully_consumed = current.stdout_cursor == stdout_total
            && current.stderr_cursor == stderr_total
            && current.stdout_carry.is_empty()
            && current.stderr_carry.is_empty();

        if fully_consumed {
            let caps = record.capacities();
            let mut candidate = current.clone();
            candidate.disposition = OutputDisposition::Consumed {
                at: jiff::Timestamp::now(),
            };
            if let Err(error) = guard.commit(candidate).await {
                if !consumed_only {
                    let caps = record.capacities();
                    let terminal_at = current.status.terminal_at().unwrap_or(record.started_at());
                    drop(guard);
                    self.register_retained(record.id(), caps, terminal_at);
                }
                return Err(error);
            }
            if let Err(error) = self.delete_rings(&guard).await {
                drop(guard);
                self.register_residual(record.id(), caps.0 + caps.1);
                return Err(error);
            }
            drop(guard);
            self.release(record.id(), caps);
        } else if !consumed_only {
            // Not fully read: keep it as an eviction victim.
            let caps = record.capacities();
            let terminal_at = current.status.terminal_at().unwrap_or(record.started_at());
            drop(guard);
            self.register_retained(record.id(), caps, terminal_at);
        }
        Ok(())
    }

    fn register_retained(&self, id: &TaskId, caps: (u64, u64), terminal_at: jiff::Timestamp) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.retained.iter().any(|entry| entry.id == *id) {
            inner.retained.push(RetainedIndexEntry {
                id: id.clone(),
                terminal_at,
                stdout_capacity: caps.0,
                stderr_capacity: caps.1,
            });
        }
    }

    fn register_residual(&self, id: &TaskId, reserved_bytes: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.retained.retain(|entry| entry.id != *id);
        if !inner.residual_deletes.iter().any(|entry| entry.id == *id) {
            inner.residual_deletes.push(ResidualIndexEntry {
                id: id.clone(),
                reserved_bytes,
            });
        }
    }

    pub fn block_spawns(&self) {
        self.inner.lock().unwrap().spawn_blocked = true;
    }

    /// Drain durable expiration facts staged by quota eviction. Staging happens
    /// synchronously with the in-memory claim transition, so cancellation can
    /// delay delivery but cannot discard the fact.
    pub fn take_expirations(&self) -> Vec<ExpirationFact> {
        std::mem::take(&mut self.inner.lock().unwrap().pending_expirations)
    }

    fn begin_activity(&self) -> QuotaActivityGuard {
        self.activity.count.send_modify(|count| *count += 1);
        QuotaActivityGuard {
            activity: self.activity.clone(),
        }
    }

    /// Wait until every owned quota transaction registered before this
    /// barrier has settled. Used by registry shutdown before its final fact
    /// drain, including transactions whose original waiter was cancelled.
    pub async fn settle(&self) {
        let mut count = self.activity.count.subscribe();
        loop {
            if *count.borrow_and_update() == 0 {
                return;
            }
            if count.changed().await.is_err() {
                return;
            }
        }
    }

    async fn delete_rings(
        &self,
        guard: &super::task_archive::TaskCommitGuard,
    ) -> Result<(), ArchiveError> {
        #[cfg(test)]
        if self.fail_next_delete.swap(false, Ordering::SeqCst) {
            return Err(ArchiveError::Io(std::io::Error::other(
                "injected ring delete failure",
            )));
        }
        #[cfg(test)]
        let pause = { self.delete_pause.lock().unwrap().take() };
        #[cfg(test)]
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
        guard.delete_rings().await
    }

    /// Release a task's reservation and drop it from the victim index.
    fn release(&self, id: &TaskId, caps: (u64, u64)) {
        let mut inner = self.inner.lock().unwrap();
        inner.reserved = inner.reserved.saturating_sub(caps.0 + caps.1);
        inner.retained.retain(|r| r.id != *id);
    }
}

enum VictimClaimState {
    Retained,
    NonReadable,
    Released,
}

/// Cancellation-safe ownership of one removed retained-index entry. Drop puts
/// it back while still Retained, or converts it to a residual after a durable
/// Consumed/Expired commit. Only an explicit successful delete releases bytes.
struct VictimClaim {
    inner: Arc<Mutex<QuotaInner>>,
    victim: Option<RetainedIndexEntry>,
    state: VictimClaimState,
}

#[cfg(test)]
struct DeletePause {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

struct QuotaActivity {
    count: watch::Sender<usize>,
}

struct QuotaActivityGuard {
    activity: Arc<QuotaActivity>,
}

impl Drop for QuotaActivityGuard {
    fn drop(&mut self) {
        self.activity.count.send_modify(|count| *count -= 1);
    }
}

impl VictimClaim {
    fn new(inner: Arc<Mutex<QuotaInner>>, victim: RetainedIndexEntry) -> Self {
        Self {
            inner,
            victim: Some(victim),
            state: VictimClaimState::Retained,
        }
    }

    fn victim(&self) -> &RetainedIndexEntry {
        self.victim.as_ref().expect("claim owns its victim")
    }

    fn mark_nonreadable(&mut self, fact: Option<ExpirationFact>) {
        if let Some(fact) = fact {
            self.inner.lock().unwrap().pending_expirations.push(fact);
        }
        self.state = VictimClaimState::NonReadable;
    }

    fn release(&mut self) {
        let reserved = self.victim().reserved();
        let mut inner = self.inner.lock().unwrap();
        inner.reserved = inner.reserved.saturating_sub(reserved);
        self.state = VictimClaimState::Released;
    }
}

impl Drop for VictimClaim {
    fn drop(&mut self) {
        let Some(victim) = self.victim.take() else {
            return;
        };
        let mut inner = self.inner.lock().unwrap();
        match self.state {
            VictimClaimState::Retained => inner.retained.push(victim),
            VictimClaimState::NonReadable => {
                let reserved_bytes = victim.reserved();
                inner.residual_deletes.push(ResidualIndexEntry {
                    id: victim.id,
                    reserved_bytes,
                });
            }
            VictimClaimState::Released => {}
        }
    }
}

/// Index of the oldest (min terminal_at) victim.
fn oldest_victim(retained: &[RetainedIndexEntry]) -> Option<usize> {
    retained
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.terminal_at.cmp(&b.terminal_at))
        .map(|(i, _)| i)
}

/// RAII reservation. If dropped without `commit()`, it rolls back the bytes it
/// reserved (the create failed after reserving); on success the reservation is
/// owned by the live task and released later by finalize/eviction.
pub struct QuotaReservation {
    lease: Arc<ReservationLease>,
}

#[derive(Clone)]
pub(crate) struct CreateReservationLease {
    lease: Arc<ReservationLease>,
}

struct ReservationLease {
    inner: Arc<Mutex<QuotaInner>>,
    bytes: u64,
    committed: AtomicBool,
}

impl QuotaReservation {
    /// Mark the reservation as owned by a successfully created task, so drop
    /// does not roll it back.
    pub fn commit(self) {
        self.lease.committed.store(true, Ordering::Release);
    }

    pub(crate) fn prepare_create(
        &self,
        required: u64,
    ) -> Result<CreateReservationLease, QuotaError> {
        if self.lease.bytes != required {
            return Err(QuotaError::LayoutMismatch {
                reserved: self.lease.bytes,
                required,
            });
        }
        Ok(CreateReservationLease {
            lease: self.lease.clone(),
        })
    }
}

impl CreateReservationLease {
    /// Preserve the charge and close the session to new growth when a detached
    /// create cannot prove its files were cleaned up.
    pub(crate) fn block_and_commit(self, error: &ArchiveError) {
        self.lease.inner.lock().unwrap().spawn_blocked = true;
        self.lease.committed.store(true, Ordering::Release);
        tracing::error!(error = %error, "task create cleanup failed; quota remains charged and spawns are blocked");
    }
}

impl Drop for ReservationLease {
    fn drop(&mut self) {
        if !self.committed.load(Ordering::Acquire)
            && let Ok(mut inner) = self.inner.lock()
        {
            inner.reserved = inner.reserved.saturating_sub(self.bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::TaskMeta;

    fn meta() -> TaskMeta {
        TaskMeta {
            command: "c".into(),
            description: "d".into(),
            agent_name: "coda".into(),
        }
    }

    fn archive() -> (tempfile::TempDir, Arc<TaskArchive>) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ArchiveDir::open_or_create_root(&tmp.path().join("background/tasks")).unwrap();
        (tmp, Arc::new(TaskArchive::new(dir)))
    }

    async fn make_terminal_retained(archive: &TaskArchive, bytes: &[u8]) -> TaskId {
        let id = TaskId::new();
        let rec = archive.create_unreserved(&id, &meta()).await.unwrap();
        rec.files().stdout.append(bytes).await.unwrap();
        rec.files().flush().await.unwrap();
        let mut g = rec.lock_commit().await;
        let mut cand = g.current().clone();
        cand.status = TaskStatus::Exited {
            code: Some(0),
            at: jiff::Timestamp::now(),
        };
        g.commit(cand).await.unwrap();
        id
    }

    #[tokio::test]
    async fn inventory_charges_retained_and_flags_orphan() {
        let (tmp, archive) = archive();
        make_terminal_retained(&archive, b"hi").await;
        // Orphan: a task-shaped dir with a ring but no manifest.
        let orphan = TaskId::new();
        let odir = archive.root().create_dir(&orphan).unwrap();
        {
            use std::io::Write;
            let mut f = odir.create_file(ArchiveFileName::StdoutRing).unwrap();
            f.write_all(&[0u8; 100]).unwrap();
        }

        let inv = scan_inventory(archive.root()).unwrap();
        assert_eq!(inv.retained.len(), 1, "one retained victim");
        assert_eq!(
            inv.retained[0].reserved(),
            DEFAULT_STREAM_CAPACITY_SUM,
            "retained charged by capacity"
        );
        assert!(inv.issue_count >= 1, "orphan flagged");
        assert!(inv.spawn_blocked, "issue sets the spawn blocker");
        assert!(inv.reserved_bytes >= DEFAULT_STREAM_CAPACITY_SUM + 100);
        let _ = tmp;
    }

    const DEFAULT_STREAM_CAPACITY_SUM: u64 =
        super::super::task_archive::DEFAULT_STREAM_CAPACITY * 2;

    #[tokio::test]
    async fn quota_evicts_oldest_terminal_first() {
        let (_tmp, archive) = archive();
        // A tiny limit that fits exactly two reservations.
        let limit = DEFAULT_STREAM_CAPACITY_SUM * 2;
        let inv = scan_inventory(archive.root()).unwrap();
        let quota = SessionQuota::from_inventory(&inv, limit, archive.clone());

        // Two terminal retained tasks, oldest first — each reserved through the
        // quota, as a real spawn would, then finished not-fully-read.
        quota
            .reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM)
            .await
            .reservation
            .unwrap()
            .commit();
        let old = make_terminal_retained(&archive, b"old").await;
        let old_rec = archive.open(&old).await.unwrap().unwrap();
        quota.finalize_terminal(&old_rec).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        quota
            .reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM)
            .await
            .reservation
            .unwrap()
            .commit();
        let new = make_terminal_retained(&archive, b"new").await;
        let new_rec = archive.open(&new).await.unwrap().unwrap();
        quota.finalize_terminal(&new_rec).await.unwrap();

        assert_eq!(quota.reserved(), DEFAULT_STREAM_CAPACITY_SUM * 2);

        // A third create needs room → evicts the oldest.
        let outcome = quota.reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM).await;
        assert_eq!(outcome.expirations.len(), 1);
        assert_eq!(outcome.expirations[0].id, old, "oldest evicted first");
        outcome.reservation.unwrap().commit();

        // The evicted task's disposition is Expired on disk.
        let reopened = archive.open(&old).await.unwrap().unwrap();
        let g = reopened.lock_commit().await;
        assert!(matches!(
            g.current().disposition,
            OutputDisposition::Expired { .. }
        ));
    }

    #[tokio::test]
    async fn fully_consumed_victim_becomes_consumed_not_expired() {
        let (_tmp, archive) = archive();
        let limit = DEFAULT_STREAM_CAPACITY_SUM; // room for exactly one
        let inv = scan_inventory(archive.root()).unwrap();
        let quota = SessionQuota::from_inventory(&inv, limit, archive.clone());

        // One terminal task, fully read (cursor == total).
        let id = TaskId::new();
        let rec = archive.create_unreserved(&id, &meta()).await.unwrap();
        rec.files().stdout.append(b"abc").await.unwrap();
        rec.files().flush().await.unwrap();
        {
            let mut g = rec.lock_commit().await;
            let mut cand = g.current().clone();
            cand.status = TaskStatus::Exited {
                code: Some(0),
                at: jiff::Timestamp::now(),
            };
            cand.stdout_cursor = 3;
            g.commit(cand).await.unwrap();
        }
        quota.finalize_terminal(&rec).await.unwrap();
        // finalize_terminal already consumed it (fully read) → reservation freed.
        assert_eq!(quota.reserved(), 0, "fully consumed released at finalize");
        let g = rec.lock_commit().await;
        assert!(matches!(
            g.current().disposition,
            OutputDisposition::Consumed { .. }
        ));
    }

    #[tokio::test]
    async fn blocked_inventory_rejects_spawn() {
        let (_tmp, archive) = archive();
        let inv = ArchiveInventory {
            spawn_blocked: true,
            ..Default::default()
        };
        let quota = SessionQuota::from_inventory(&inv, SESSION_QUOTA_BYTES, archive);
        assert!(matches!(
            quota.reserve_for_test(1024).await.reservation,
            Err(QuotaError::Blocked)
        ));
    }

    #[tokio::test]
    async fn delete_failure_stays_charged_and_retries_without_duplicate_fact() {
        let (_tmp, archive) = archive();
        let limit = DEFAULT_STREAM_CAPACITY_SUM;
        let quota = SessionQuota::from_inventory(
            &scan_inventory(archive.root()).unwrap(),
            limit,
            archive.clone(),
        );
        quota
            .reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM)
            .await
            .reservation
            .unwrap()
            .commit();
        let id = make_terminal_retained(&archive, b"unread").await;
        let record = archive.open(&id).await.unwrap().unwrap();
        quota.finalize_terminal(&record).await.unwrap();

        quota.fail_next_delete();
        let failed = quota.reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM).await;
        assert!(matches!(failed.reservation, Err(QuotaError::Archive(_))));
        assert_eq!(failed.expirations.len(), 1);
        assert_eq!(failed.expirations[0].id, id);
        assert_eq!(quota.reserved(), DEFAULT_STREAM_CAPACITY_SUM);
        assert_eq!(quota.inner.lock().unwrap().residual_deletes.len(), 1);

        let retried = quota.reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM).await;
        assert!(retried.expirations.is_empty(), "expiration is emitted once");
        assert!(retried.reservation.is_ok());
        assert_eq!(quota.reserved(), DEFAULT_STREAM_CAPACITY_SUM);
        drop(retried.reservation);
        assert_eq!(quota.reserved(), 0);
    }

    #[tokio::test]
    async fn eviction_commit_failure_restores_the_victim() {
        use std::os::unix::fs::PermissionsExt;

        let (tmp, archive) = archive();
        let limit = DEFAULT_STREAM_CAPACITY_SUM;
        let quota = SessionQuota::from_inventory(
            &scan_inventory(archive.root()).unwrap(),
            limit,
            archive.clone(),
        );
        quota
            .reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM)
            .await
            .reservation
            .unwrap()
            .commit();
        let id = make_terminal_retained(&archive, b"unread").await;
        let record = archive.open(&id).await.unwrap().unwrap();
        quota.finalize_terminal(&record).await.unwrap();

        let task_path = tmp.path().join("background/tasks").join(id.as_str());
        std::fs::set_permissions(&task_path, std::fs::Permissions::from_mode(0o500)).unwrap();
        let failed = quota.reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM).await;
        assert!(matches!(failed.reservation, Err(QuotaError::Archive(_))));
        assert!(failed.expirations.is_empty());
        assert!(quota.retained_contains(&id));
        assert_eq!(quota.reserved(), DEFAULT_STREAM_CAPACITY_SUM);

        std::fs::set_permissions(&task_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let retried = quota.reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM).await;
        assert_eq!(retried.expirations.len(), 1);
        assert!(retried.reservation.is_ok());
    }

    #[tokio::test]
    async fn terminal_utf8_carry_keeps_output_retained() {
        let (_tmp, archive) = archive();
        let quota = SessionQuota::from_inventory(
            &scan_inventory(archive.root()).unwrap(),
            SESSION_QUOTA_BYTES,
            archive.clone(),
        );
        quota
            .reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM)
            .await
            .reservation
            .unwrap()
            .commit();
        let id = TaskId::new();
        let record = archive.create_unreserved(&id, &meta()).await.unwrap();
        record.files().stdout.append(&[0xE2]).await.unwrap();
        record.files().flush().await.unwrap();
        {
            let mut guard = record.lock_commit().await;
            let mut candidate = guard.current().clone();
            candidate.status = TaskStatus::Exited {
                code: Some(0),
                at: jiff::Timestamp::now(),
            };
            candidate.stdout_cursor = 1;
            candidate.stdout_carry = vec![0xE2];
            guard.commit(candidate).await.unwrap();
        }
        quota.finalize_terminal(&record).await.unwrap();
        let guard = record.lock_commit().await;
        assert_eq!(guard.current().disposition, OutputDisposition::Retained);
        drop(guard);
        assert!(quota.retained_contains(&id));
        assert_eq!(quota.reserved(), DEFAULT_STREAM_CAPACITY_SUM);
    }

    #[tokio::test]
    async fn inventory_rejects_terminal_retained_with_missing_ring() {
        let (_tmp, archive) = archive();
        let id = make_terminal_retained(&archive, b"output").await;
        let task_dir = archive.root().open_dir(&id).unwrap();
        task_dir.unlink(ArchiveFileName::StderrRing).unwrap();

        let inventory = scan_inventory(archive.root()).unwrap();
        assert!(inventory.spawn_blocked);
        assert_eq!(inventory.retained_count, 0);
        assert_eq!(inventory.issue_count, 1);
    }

    #[tokio::test]
    async fn recoverable_running_inventory_is_strictly_bounded() {
        let (_tmp, archive) = archive();
        for _ in 0..=MAX_RECOVERABLE_RUNNING {
            let id = TaskId::new();
            archive.create_unreserved(&id, &meta()).await.unwrap();
        }

        let inventory = scan_inventory(archive.root()).unwrap();
        assert_eq!(inventory.recoverable_running.len(), MAX_RECOVERABLE_RUNNING);
        assert_eq!(inventory.issue_count, 1);
        assert!(inventory.spawn_blocked);
    }

    #[tokio::test]
    async fn cancelled_reserve_waiter_does_not_abandon_claimed_victim() {
        let (_tmp, archive) = archive();
        let quota = Arc::new(SessionQuota::from_inventory(
            &scan_inventory(archive.root()).unwrap(),
            DEFAULT_STREAM_CAPACITY_SUM,
            archive.clone(),
        ));
        quota
            .reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM)
            .await
            .reservation
            .unwrap()
            .commit();
        let id = make_terminal_retained(&archive, b"unread").await;
        let record = archive.open(&id).await.unwrap().unwrap();
        quota.finalize_terminal(&record).await.unwrap();
        let commit_guard = record.lock_commit().await;

        let reserve_quota = quota.clone();
        let reserve = tokio::spawn(async move {
            reserve_quota
                .reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while quota.retained_contains(&id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("quota never claimed its victim");
        reserve.abort();
        assert!(matches!(reserve.await, Err(error) if error.is_cancelled()));
        drop(commit_guard);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while quota.reserved() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned quota transaction did not finish after waiter cancellation");
        assert!(!quota.retained_contains(&id));
        let facts = quota.take_expirations();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, id);
    }

    #[tokio::test]
    async fn cancelled_reserve_waiter_finishes_committed_eviction() {
        let (_tmp, archive) = archive();
        let quota = Arc::new(SessionQuota::from_inventory(
            &scan_inventory(archive.root()).unwrap(),
            DEFAULT_STREAM_CAPACITY_SUM,
            archive.clone(),
        ));
        quota
            .reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM)
            .await
            .reservation
            .unwrap()
            .commit();
        let id = make_terminal_retained(&archive, b"unread").await;
        let record = archive.open(&id).await.unwrap().unwrap();
        quota.finalize_terminal(&record).await.unwrap();
        let (entered, release) = quota.pause_next_delete();

        let reserve_quota = quota.clone();
        let reserve = tokio::spawn(async move {
            reserve_quota
                .reserve_for_test(DEFAULT_STREAM_CAPACITY_SUM)
                .await
        });
        entered.notified().await;
        reserve.abort();
        assert!(matches!(reserve.await, Err(error) if error.is_cancelled()));
        release.notify_one();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while quota.reserved() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned eviction did not finish after waiter cancellation");
        assert!(quota.inner.lock().unwrap().residual_deletes.is_empty());
        let facts = quota.take_expirations();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, id);
    }

    #[tokio::test]
    async fn recent_terminal_inventory_is_compacted_at_end_of_scan() {
        let (_tmp, archive) = archive();
        for _ in 0..40 {
            make_terminal_retained(&archive, b"x").await;
        }
        let inventory = scan_inventory(archive.root()).unwrap();
        assert_eq!(inventory.recent_terminal.len(), MAX_RECENT_TERMINAL);
    }

    #[tokio::test]
    async fn settle_cannot_miss_last_activity_transition() {
        let (_tmp, archive) = archive();
        let quota = Arc::new(SessionQuota::from_inventory(
            &scan_inventory(archive.root()).unwrap(),
            SESSION_QUOTA_BYTES,
            archive,
        ));
        for _ in 0..100 {
            let activity = quota.begin_activity();
            let settle_quota = quota.clone();
            let settle = tokio::spawn(async move { settle_quota.settle().await });
            tokio::task::yield_now().await;
            drop(activity);
            tokio::time::timeout(std::time::Duration::from_millis(100), settle)
                .await
                .expect("settle missed the final activity transition")
                .unwrap();
        }
    }
}
