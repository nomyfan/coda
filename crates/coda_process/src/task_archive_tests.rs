use super::*;
use crate::quota::{QuotaError, SESSION_QUOTA_BYTES, SessionQuota, scan_inventory};

fn meta() -> TaskMeta {
    TaskMeta {
        command: "echo hi".into(),
        description: "d".into(),
        agent_name: "coda".into(),
    }
}

fn root() -> (tempfile::TempDir, TaskArchive) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
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
async fn oversized_initial_manifest_is_rejected_and_rolled_back() {
    let (_tmp, archive) = root();
    let id = TaskId::new();
    let oversized = TaskMeta {
        command: "x".repeat(MAX_MANIFEST_BYTES as usize),
        description: "d".into(),
        agent_name: "coda".into(),
    };

    let error = match archive.create_unreserved(&id, &oversized).await {
        Ok(_) => panic!("writer accepted a manifest its reader rejects"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("over the 65536 cap"));
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
        tokio::spawn(async move { create_archive.create_unreserved(&create_id, &meta()).await });
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
