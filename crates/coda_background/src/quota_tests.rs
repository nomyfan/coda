use super::*;
use crate::TaskMeta;

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

const DEFAULT_STREAM_CAPACITY_SUM: u64 = super::super::task_archive::DEFAULT_STREAM_CAPACITY * 2;

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
