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
//! terminal-Retained victims when a create needs room. The counter/index/blocker
//! live behind a *leaf* `std::sync::Mutex`: a create claims its victims under it
//! and then performs the manifest-first eviction I/O outside it, per-victim under
//! that victim's commit lock. The mutex is never held across a commit-lock
//! acquisition, so the global order stays acyclic (quota decision ⟶ per-task
//! commit) with no async lock held across `.await`.

use std::sync::{Arc, Mutex};

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
    inv.spawn_blocked = inv.issue_count > 0
        || inv.retained_index_truncated
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
                inv.recoverable_running.push(id);
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
                }
                OutputDisposition::Consumed { .. } | OutputDisposition::Expired { .. } => {
                    // Rings should be gone; charge any that a failed delete left.
                    inv.reserved_bytes += charge_ring_files(task_dir);
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
    Archive(ArchiveError),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::Blocked => f.write_str(
                "background archive inventory blocked new spawns (corrupt or over-quota state)",
            ),
            QuotaError::Exhausted => f.write_str("session output quota exhausted"),
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

/// Result of a successful reservation, plus any expirations it caused.
pub struct ReserveOutcome {
    pub reservation: QuotaReservation,
    pub expirations: Vec<ExpirationFact>,
}

struct QuotaInner {
    reserved: u64,
    retained: Vec<RetainedIndexEntry>,
    spawn_blocked: bool,
}

/// Serializes create/eviction/release for one session's ring payload.
pub struct SessionQuota {
    limit: u64,
    inner: Arc<Mutex<QuotaInner>>,
    archive: Arc<TaskArchive>,
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
            inner: Arc::new(Mutex::new(QuotaInner {
                reserved: inventory.reserved_bytes,
                retained: inventory.retained.clone(),
                spawn_blocked: inventory.spawn_blocked,
            })),
            archive,
        }
    }

    #[cfg(test)]
    fn reserved(&self) -> u64 {
        self.inner.lock().unwrap().reserved
    }

    /// Reserve `bytes` for a new task, evicting oldest terminal victims as
    /// needed. The reservation decision is serialized under the leaf mutex; the
    /// manifest-first eviction I/O then runs per-victim under each victim's
    /// commit lock (no lock held across it). Returns the reservation guard plus
    /// the expiration facts for the registry to enqueue.
    pub async fn reserve_for_create(&self, bytes: u64) -> Result<ReserveOutcome, QuotaError> {
        // Phase 1: claim victims and the reservation under the leaf mutex.
        let victims = {
            let mut inner = self.inner.lock().unwrap();
            if inner.spawn_blocked {
                return Err(QuotaError::Blocked);
            }
            let mut claimed = Vec::new();
            while inner.reserved + bytes > self.limit {
                let Some(idx) = oldest_victim(&inner.retained) else {
                    // Roll back provisional claims: put them back.
                    for v in claimed.drain(..) {
                        let v: RetainedIndexEntry = v;
                        inner.reserved += v.reserved();
                        inner.retained.push(v);
                    }
                    return Err(QuotaError::Exhausted);
                };
                let victim = inner.retained.swap_remove(idx);
                inner.reserved -= victim.reserved();
                claimed.push(victim);
            }
            inner.reserved += bytes;
            claimed
        };

        // Phase 2: perform manifest-first eviction outside the mutex.
        let mut expirations = Vec::new();
        for victim in victims {
            if let Some(fact) = self.evict_victim(&victim).await? {
                expirations.push(fact);
            }
        }

        Ok(ReserveOutcome {
            reservation: QuotaReservation {
                inner: self.inner.clone(),
                bytes,
                committed: false,
            },
            expirations,
        })
    }

    /// Evict one claimed victim: reopen it, recheck under its commit lock, and
    /// commit Consumed (if fully read) or Expired, then delete the rings. A
    /// fully-consumed victim yields no expiration fact.
    async fn evict_victim(
        &self,
        victim: &RetainedIndexEntry,
    ) -> Result<Option<ExpirationFact>, QuotaError> {
        let Some(record) = self.archive.open(&victim.id).await? else {
            return Ok(None); // already gone
        };
        let mut guard = record.lock_commit().await;
        let current = guard.current().clone();
        // Only a still-Retained terminal is evictable.
        if current.disposition != OutputDisposition::Retained || current.status.is_running() {
            return Ok(None);
        }
        let now = jiff::Timestamp::now();
        let fully_consumed = current.stdout_cursor == record.files().stdout.logical_range().await.1
            && current.stderr_cursor == record.files().stderr.logical_range().await.1;

        let mut candidate = current;
        if fully_consumed {
            candidate.disposition = OutputDisposition::Consumed { at: now };
            guard.commit(candidate).await?;
            guard.delete_rings().await?;
            Ok(None)
        } else {
            let reason = super::manifest::ExpireReason::SessionQuota;
            candidate.disposition = OutputDisposition::Expired { at: now, reason };
            guard.commit(candidate).await?;
            guard.delete_rings().await?;
            Ok(Some(ExpirationFact {
                id: victim.id.clone(),
                expired_at: now,
                reason,
            }))
        }
    }

    /// After a task's terminal manifest is committed (commit lock released),
    /// recheck: fully consumed → Consumed cleanup + release; otherwise register
    /// it as an evictable victim (its reservation stays until evicted/consumed).
    pub async fn finalize_terminal(&self, record: &TaskRecord) -> Result<(), ArchiveError> {
        self.finalize(record, false).await
    }

    /// After a cursor commit, release a terminal task that just became fully
    /// consumed. A no-op while Running or not yet fully read.
    pub async fn finalize_consumed(&self, record: &TaskRecord) -> Result<(), ArchiveError> {
        self.finalize(record, true).await
    }

    async fn finalize(&self, record: &TaskRecord, consumed_only: bool) -> Result<(), ArchiveError> {
        let mut guard = record.lock_commit().await;
        let current = guard.current().clone();
        if current.status.is_running() || current.disposition != OutputDisposition::Retained {
            return Ok(());
        }
        let stdout_total = record.files().stdout.logical_range().await.1;
        let stderr_total = record.files().stderr.logical_range().await.1;
        let fully_consumed =
            current.stdout_cursor == stdout_total && current.stderr_cursor == stderr_total;

        if fully_consumed {
            let caps = record.capacities();
            let mut candidate = current;
            candidate.disposition = OutputDisposition::Consumed {
                at: jiff::Timestamp::now(),
            };
            guard.commit(candidate).await?;
            guard.delete_rings().await?;
            drop(guard);
            self.release(record.id(), caps);
        } else if !consumed_only {
            // Not fully read: keep it as an eviction victim.
            let caps = record.capacities();
            let terminal_at = current.status.terminal_at().unwrap_or(record.started_at());
            drop(guard);
            let mut inner = self.inner.lock().unwrap();
            if !inner.retained.iter().any(|r| r.id == *record.id()) {
                inner.retained.push(RetainedIndexEntry {
                    id: record.id().clone(),
                    terminal_at,
                    stdout_capacity: caps.0,
                    stderr_capacity: caps.1,
                });
            }
        }
        Ok(())
    }

    /// Release a task's reservation and drop it from the victim index.
    fn release(&self, id: &TaskId, caps: (u64, u64)) {
        let mut inner = self.inner.lock().unwrap();
        inner.reserved = inner.reserved.saturating_sub(caps.0 + caps.1);
        inner.retained.retain(|r| r.id != *id);
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
    inner: Arc<Mutex<QuotaInner>>,
    bytes: u64,
    committed: bool,
}

impl QuotaReservation {
    /// Mark the reservation as owned by a successfully created task, so drop
    /// does not roll it back.
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for QuotaReservation {
    fn drop(&mut self) {
        if !self.committed
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
        let rec = archive.create(&id, &meta()).await.unwrap();
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
            .reserve_for_create(DEFAULT_STREAM_CAPACITY_SUM)
            .await
            .unwrap()
            .reservation
            .commit();
        let old = make_terminal_retained(&archive, b"old").await;
        let old_rec = archive.open(&old).await.unwrap().unwrap();
        quota.finalize_terminal(&old_rec).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        quota
            .reserve_for_create(DEFAULT_STREAM_CAPACITY_SUM)
            .await
            .unwrap()
            .reservation
            .commit();
        let new = make_terminal_retained(&archive, b"new").await;
        let new_rec = archive.open(&new).await.unwrap().unwrap();
        quota.finalize_terminal(&new_rec).await.unwrap();

        assert_eq!(quota.reserved(), DEFAULT_STREAM_CAPACITY_SUM * 2);

        // A third create needs room → evicts the oldest.
        let outcome = quota
            .reserve_for_create(DEFAULT_STREAM_CAPACITY_SUM)
            .await
            .unwrap();
        assert_eq!(outcome.expirations.len(), 1);
        assert_eq!(outcome.expirations[0].id, old, "oldest evicted first");
        outcome.reservation.commit();

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
        let rec = archive.create(&id, &meta()).await.unwrap();
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
            quota.reserve_for_create(1024).await,
            Err(QuotaError::Blocked)
        ));
    }
}
