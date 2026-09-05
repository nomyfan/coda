use super::*;
use crate::DEFAULT_STREAM_CAPACITY;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// The ring capacity a fresh stream is created with — the retained window.
const TAIL_BUF_CAP: usize = DEFAULT_STREAM_CAPACITY as usize;

fn meta(command: &str) -> TaskMeta {
    TaskMeta::shell(command.into(), "test task".into(), "coda".into())
}

fn running_count(rx: &watch::Receiver<Arc<[TaskSummary]>>) -> usize {
    rx.borrow().iter().filter(|s| s.status.is_running()).count()
}

/// spawn publishes the running task before returning the id.
#[tokio::test]
async fn spawn_publishes_keepalive_before_returning() {
    let reg = BackgroundTasks::temporary().unwrap();
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
    let reg = BackgroundTasks::temporary().unwrap();
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
    let reg = BackgroundTasks::temporary().unwrap();
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
    let reg = BackgroundTasks::temporary().unwrap();
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
    let reg = BackgroundTasks::temporary().unwrap();
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

#[tokio::test]
async fn rejected_process_spawn_has_no_command_side_effects() {
    let reg = BackgroundTasks::temporary().unwrap();
    reg.shutdown().await;
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("should-not-exist");
    let mut cmd = bash("printf x > \"$MARKER\"");
    cmd.env("MARKER", &marker);

    let error = reg.spawn(cmd, meta("rejected")).await.unwrap_err();
    assert!(error.to_string().contains("shut down"));
    assert!(!marker.exists(), "the rejected command was never started");
}

#[tokio::test]
async fn process_start_failure_rolls_back_archive_and_quota() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let archive = Arc::new(TaskArchive::new(root));
    let (acknowledged, release) = archive.pause_after_next_create_ack();
    let quota = SessionQuota::from_inventory(
        &ArchiveInventory::default(),
        SESSION_QUOTA_BYTES,
        archive.clone(),
    );
    let reg = Arc::new(BackgroundTasks::new(Arc::new(Backend {
        archive: archive.clone(),
        quota,
        temp: None,
    })));

    let cmd = Command::new("/definitely/not/a/real/executable");
    let spawn_reg = reg.clone();
    let spawn = tokio::spawn(async move { spawn_reg.spawn(cmd, meta("bad executable")).await });
    acknowledged.notified().await;
    let error = tokio::time::timeout(Duration::from_secs(2), spawn)
        .await
        .expect("process-start failure waited for the acknowledged create transaction")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(archive.root().entries().unwrap().count(), 0);
    assert_eq!(reg.backend.quota.reserved(), 0);
    release.notify_one();
    archive.settle().await;
}

#[tokio::test]
async fn oversized_task_metadata_is_rejected_before_work_starts() {
    let reg = BackgroundTasks::temporary().unwrap();
    let started = Arc::new(AtomicBool::new(false));
    let work_started = started.clone();
    let error = reg
        .spawn_with(
            TaskMeta::shell("x".repeat(64 * 1024), "too large".into(), "coda".into()),
            move |_ctx| {
                work_started.store(true, Ordering::SeqCst);
                async { TaskExit::Exited { code: Some(0) } }
            },
        )
        .await
        .expect_err("oversized manifest was accepted");
    assert!(error.to_string().contains("over the 65536 cap"));
    assert!(
        !started.load(Ordering::SeqCst),
        "work was started before validation"
    );
}

#[tokio::test]
async fn shutdown_waits_for_detached_create_transaction() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let archive = Arc::new(TaskArchive::new(root));
    let quota = SessionQuota::from_inventory(
        &ArchiveInventory::default(),
        SESSION_QUOTA_BYTES,
        archive.clone(),
    );
    let reg = Arc::new(BackgroundTasks::new(Arc::new(Backend {
        archive: archive.clone(),
        quota,
        temp: None,
    })));
    let (entered, release) = archive.pause_next_create();
    let spawn_reg = reg.clone();
    let spawn = tokio::spawn(async move {
        spawn_reg
            .spawn_with(meta("cancelled create"), |_ctx| async {
                TaskExit::Exited { code: Some(0) }
            })
            .await
    });
    entered.notified().await;
    spawn.abort();
    assert!(matches!(spawn.await, Err(error) if error.is_cancelled()));

    let shutdown_reg = reg.clone();
    let mut shutdown = tokio::spawn(async move { shutdown_reg.shutdown().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
            .await
            .is_err(),
        "shutdown returned before detached create settled"
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown did not resume after create settled")
        .unwrap();
    assert_eq!(archive.root().entries().unwrap().count(), 0);
}

#[tokio::test]
async fn quiescent_drain_waits_for_detached_quota_expiration() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let archive = Arc::new(TaskArchive::new(root));
    let quota = SessionQuota::from_inventory(
        &ArchiveInventory::default(),
        2 * DEFAULT_STREAM_CAPACITY,
        archive.clone(),
    );
    let reg = Arc::new(BackgroundTasks::new(Arc::new(Backend {
        archive,
        quota: quota.clone(),
        temp: None,
    })));

    let old = reg
        .spawn_with(meta("old"), |ctx| async move {
            ctx.append_stdout(b"unread").await.unwrap();
            TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    let mut summaries = reg.summaries();
    while running_count(&summaries) != 0 {
        summaries.changed().await.unwrap();
    }
    reg.take_notices().await;

    let (delete_entered, release_delete) = quota.pause_next_delete();
    let spawn_reg = reg.clone();
    let spawn = tokio::spawn(async move {
        spawn_reg
            .spawn_with(meta("cancelled replacement"), |_ctx| async {
                TaskExit::Exited { code: Some(0) }
            })
            .await
    });
    delete_entered.notified().await;
    spawn.abort();
    assert!(matches!(spawn.await, Err(error) if error.is_cancelled()));

    let drain_reg = reg.clone();
    let mut drain = tokio::spawn(async move { drain_reg.take_notices_if_quiescent().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut drain)
            .await
            .is_err(),
        "quiescent drain returned before detached quota work settled"
    );
    release_delete.notify_one();
    let notices = tokio::time::timeout(Duration::from_secs(2), drain)
        .await
        .expect("quiescent drain did not resume")
        .unwrap()
        .expect("no task is running");
    assert!(notices.iter().any(|notice| matches!(
        notice,
        TaskNotice::OutputExpired { id, .. } if id == &old
    )));
}

#[tokio::test]
async fn normal_shutdown_preserves_killed_output_without_replaying_notice() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let reg = BackgroundTasks::session_backed(root.clone()).await.unwrap();
    let id = reg
        .spawn_with(meta("restart"), |ctx| async move {
            ctx.append_stdout(b"before restart").await.unwrap();
            ctx.cancelled().cancelled().await;
            TaskExit::Killed
        })
        .await
        .unwrap();

    let shutdown_notices = reg.shutdown().await;
    assert!(shutdown_notices.iter().any(|notice| matches!(
        notice,
        TaskNotice::Task { id: notice_id, status: TaskStatus::Killed { .. }, .. }
            if notice_id == &id
    )));
    drop(reg);

    let reopened = BackgroundTasks::session_backed(root).await.unwrap();
    assert!(
        reopened.take_notices().await.is_empty(),
        "normal restart unexpectedly replayed a completion notice"
    );
    let read = reopened.read(&id).await.unwrap().unwrap();
    assert!(matches!(read.status, TaskStatus::Killed { .. }));
    assert_eq!(read.stdout, "before restart");
    reopened.shutdown().await;
}

#[tokio::test]
async fn create_failure_does_not_lose_prior_expiration_fact() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let archive = Arc::new(TaskArchive::new(root));
    let quota = SessionQuota::from_inventory(
        &ArchiveInventory::default(),
        2 * DEFAULT_STREAM_CAPACITY,
        archive.clone(),
    );
    let reg = BackgroundTasks::new(Arc::new(Backend {
        archive: archive.clone(),
        quota,
        temp: None,
    }));
    let old = reg
        .spawn_with(meta("old"), |ctx| async move {
            ctx.append_stdout(b"unread").await.unwrap();
            TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    let mut rx = reg.summaries();
    while running_count(&rx) > 0 {
        rx.changed().await.unwrap();
    }

    archive.fail_next_initial_manifest();
    assert!(
        reg.spawn_with(meta("create fails"), |_ctx| async {
            TaskExit::Exited { code: Some(0) }
        })
        .await
        .is_err()
    );
    let notices = reg.take_notices().await;
    assert!(notices.iter().any(|notice| matches!(
        notice,
        TaskNotice::OutputExpired { id, .. } if id == &old
    )));
    archive.settle().await;
    assert_eq!(reg.backend.quota.reserved(), 0);
}

#[tokio::test]
async fn terminal_read_flushes_incomplete_utf8_before_consuming_output() {
    let reg = BackgroundTasks::temporary().unwrap();
    let ready = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let task_ready = ready.clone();
    let task_finish = finish.clone();
    let id = reg
        .spawn_with(meta("partial utf8"), move |ctx| async move {
            ctx.append_stdout(&[0xE2]).await.unwrap();
            task_ready.notify_one();
            task_finish.notified().await;
            TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();

    ready.notified().await;
    let running = reg.read(&id).await.unwrap().unwrap();
    assert!(running.stdout.is_empty(), "incomplete prefix is carried");
    finish.notify_one();
    let mut rx = reg.summaries();
    while running_count(&rx) > 0 {
        rx.changed().await.unwrap();
    }

    let terminal = reg.read(&id).await.unwrap().unwrap();
    assert_eq!(terminal.stdout, "\u{FFFD}");
    assert!(terminal.note.is_none());
    let consumed = reg.read(&id).await.unwrap().unwrap();
    assert!(consumed.note.as_deref().unwrap().contains("fully consumed"));
}

#[tokio::test]
async fn reopen_finalizes_interrupted_task_into_quota_index() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let archive = TaskArchive::new(root.clone());
    let id = TaskId::new();
    let record = archive
        .create_unreserved(&id, &meta("crashed"))
        .await
        .unwrap();
    record.files().stdout.append(b"saved").await.unwrap();
    record.files().flush().await.unwrap();
    {
        let mut guard = record.lock_commit().await;
        let candidate = guard.current().clone();
        guard.commit(candidate).await.unwrap();
    }
    drop(record);
    drop(archive);

    let reg = BackgroundTasks::session_backed(root).await.unwrap();
    let backend = &reg.backend;
    assert!(backend.quota.retained_contains(&id));
    assert_eq!(backend.quota.reserved(), 2 * DEFAULT_STREAM_CAPACITY);
    let read = reg.read(&id).await.unwrap().unwrap();
    assert!(matches!(read.status, TaskStatus::Interrupted { .. }));
    assert_eq!(read.stdout, "saved");

    // Dying with the server is still a way for a task to end, so it owes the
    // model a notice — the same one a normal completion would produce.
    let notices = reg.take_notices().await;
    let TaskNotice::Task {
        id: noticed,
        status,
        output_tail,
        ..
    } = notices.first().expect("an interrupted task notifies")
    else {
        panic!("unexpected notice: {:?}", notices[0]);
    };
    assert_eq!(noticed, &id);
    assert!(matches!(status, TaskStatus::Interrupted { .. }));
    assert_eq!(output_tail, "saved");
}

#[tokio::test]
async fn shutdown_retries_dirty_in_memory_failed_manifest() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let archive = Arc::new(TaskArchive::new(root.clone()));
    let quota = SessionQuota::from_inventory(
        &ArchiveInventory::default(),
        SESSION_QUOTA_BYTES,
        archive.clone(),
    );
    let reg = BackgroundTasks::new(Arc::new(Backend {
        archive: archive.clone(),
        quota,
        temp: None,
    }));
    let finish = Arc::new(Notify::new());
    let task_finish = finish.clone();
    let id = reg
        .spawn_with(meta("manifest failure"), move |_ctx| async move {
            task_finish.notified().await;
            TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    let task_path = tmp.path().join(id.as_str());
    std::fs::set_permissions(&task_path, std::fs::Permissions::from_mode(0o500)).unwrap();
    finish.notify_one();
    let mut rx = reg.summaries();
    while running_count(&rx) > 0 {
        rx.changed().await.unwrap();
    }
    let summary = rx
        .borrow()
        .iter()
        .find(|summary| summary.id == id.as_str())
        .cloned();
    assert!(matches!(summary.unwrap().status, TaskStatus::Failed { .. }));
    let record = reg
        .inner
        .lock()
        .await
        .tasks
        .get(&id)
        .unwrap()
        .record
        .clone();
    assert!(matches!(
        record.lock_commit().await.current().status,
        TaskStatus::Failed { .. }
    ));
    std::fs::set_permissions(&task_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    reg.shutdown().await;

    let reopened = TaskArchive::new(root).open(&id).await.unwrap().unwrap();
    assert!(matches!(
        reopened.lock_commit().await.current().status,
        TaskStatus::Failed { .. }
    ));
}

/// Incremental reads move an absolute cursor; a truncated head is
/// reported as lost bytes, never re-read or skipped.
#[tokio::test]
async fn read_reports_lost_bytes_after_truncation() {
    let reg = BackgroundTasks::temporary().unwrap();
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
    reg.kill(&id).await.unwrap();
    let read = reg.read(&id).await.unwrap().unwrap();
    assert_eq!(read.stdout.len(), 0);
    assert_eq!(read.stdout_lost, 0);
    assert!(!read.complete, "loss while running still counts after exit");
    reg.shutdown().await;
}

#[tokio::test]
async fn paginated_loss_prevents_complete_reads_even_after_reopen() {
    for stderr in [false, true] {
        for reopen in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
            let mut reg = BackgroundTasks::session_backed(root.clone()).await.unwrap();
            let id = reg
                .spawn_with(meta("paginated loss"), move |ctx| async move {
                    let bytes = vec![b'x'; TAIL_BUF_CAP + 7];
                    if stderr {
                        ctx.append_stderr(&bytes).await.unwrap();
                    } else {
                        ctx.append_stdout(&bytes).await.unwrap();
                    }
                    TaskExit::Exited { code: Some(0) }
                })
                .await
                .unwrap();
            reg.wait_terminal(&id).await;

            let first = reg.read(&id).await.unwrap().unwrap();
            assert_eq!(first.stdout_lost + first.stderr_lost, 7);
            assert!(!first.complete);
            let mut drained = first.stdout.len() + first.stderr.len();
            if reopen {
                reg.shutdown().await;
                drop(reg);
                reg = BackgroundTasks::session_backed(root).await.unwrap();
            }
            while drained < TAIL_BUF_CAP {
                let page = reg.read(&id).await.unwrap().unwrap();
                assert_eq!(page.stdout_lost + page.stderr_lost, 0);
                let bytes = page.stdout.len() + page.stderr.len();
                assert!(bytes > 0);
                drained += bytes;
                assert!(
                    !page.complete,
                    "earlier loss must survive pagination: stderr={stderr}, reopen={reopen}, drained={drained}"
                );
            }
            assert_eq!(drained, TAIL_BUF_CAP);
            reg.shutdown().await;
        }
    }
}

/// Reopened summaries and newly interrupted crash leftovers occupy the same
/// bounded terminal overview as tasks completed by the new process.
#[tokio::test]
async fn recovered_terminal_entries_share_the_live_overview_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let archive = TaskArchive::new(root.clone());
    let mut oldest = None;

    // Seed 31 durable terminal tasks plus one crash-Running task. Reopen must
    // put both kinds into the same oldest-first eviction queue.
    for i in 0..(MAX_TERMINAL - 1) {
        let id = TaskId::new();
        let record = archive
            .create_unreserved(&id, &meta(&format!("restored-{i}")))
            .await
            .unwrap();
        let mut guard = record.lock_commit().await;
        let mut candidate = guard.current().clone();
        candidate.status = TaskStatus::Exited {
            code: Some(0),
            at: format!("2020-01-01T00:00:{i:02}Z").parse().unwrap(),
        };
        guard.commit(candidate).await.unwrap();
        oldest.get_or_insert(id);
    }
    let oldest = oldest.unwrap();
    let interrupted = TaskId::new();
    archive
        .create_unreserved(&interrupted, &meta("crashed"))
        .await
        .unwrap();
    drop(archive);

    let reg = BackgroundTasks::session_backed(root).await.unwrap();
    assert_eq!(reg.summaries().borrow().len(), MAX_TERMINAL);
    assert!(
        reg.summaries()
            .borrow()
            .iter()
            .any(|summary| summary.id == interrupted.as_str())
    );

    let newest = reg
        .spawn_with(meta("newest"), |_ctx| async {
            TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    let mut summaries = reg.summaries();
    loop {
        if summaries
            .borrow_and_update()
            .iter()
            .any(|summary| summary.id == newest.as_str() && !summary.status.is_running())
        {
            break;
        }
        summaries.changed().await.unwrap();
    }

    let summaries = summaries.borrow();
    assert_eq!(summaries.len(), MAX_TERMINAL);
    assert!(
        !summaries
            .iter()
            .any(|summary| summary.id == oldest.as_str())
    );
    assert!(
        summaries
            .iter()
            .any(|summary| summary.id == interrupted.as_str())
    );
    drop(summaries);
    reg.shutdown().await;
}

/// Terminal entries beyond MAX_TERMINAL are reclaimed from the in-memory
/// live overview oldest-first, but stay readable by id from the archive:
/// memory reclamation is decoupled from disk retention.
#[tokio::test]
async fn terminal_entries_are_reclaimed_beyond_cap() {
    let reg = BackgroundTasks::temporary().unwrap();
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
    let reg = BackgroundTasks::temporary().unwrap();
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

/// Concurrent shutdowns serialize on the teardown barrier: the notice of
/// the killed task lands in exactly one drain, and neither call returns
/// with work still running.
#[tokio::test]
async fn concurrent_shutdowns_share_the_barrier() {
    let reg = Arc::new(BackgroundTasks::temporary().unwrap());
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

/// A chatty real process producing far more than one ring's worth of output
/// streams end-to-end without stalling: incremental reads drain a bounded
/// window, and the completion notice reports the storage-level overwrite.
#[tokio::test]
async fn chatty_process_overflows_ring_and_reports_overwrite() {
    let reg = BackgroundTasks::temporary().unwrap();
    // ~10 bytes/line × 200_000 ≈ 2 MiB, well past the 512 KiB ring.
    let id = reg
        .spawn(
            bash("for i in $(seq 1 200000); do echo \"ln $i\"; done"),
            meta("chatty"),
        )
        .await
        .unwrap();

    // Drain incrementally until the process settles; reads must not block.
    let mut seen = 0usize;
    let status = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let read = reg.read(&id).await.unwrap().expect("task known");
            seen += read.stdout.len();
            if !read.status.is_running() && read.stdout.is_empty() {
                break read.status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("chatty process never settled");
    assert!(matches!(status, TaskStatus::Exited { code: Some(0), .. }));
    assert!(seen > 0, "streamed some output");

    let notices = reg.take_notices().await;
    let overwritten = notices.iter().find_map(|n| match n {
        TaskNotice::Task {
            stdout_overwritten, ..
        } => Some(*stdout_overwritten),
        _ => None,
    });
    assert!(
        overwritten.is_some_and(|o| o > 0),
        "producing >1 ring of output overwrote earlier bytes: {overwritten:?}"
    );
    reg.shutdown().await;
}

/// spawn → incremental reads observe streamed output → natural exit
/// commits the code and produces a notice carrying the tail.
#[tokio::test]
async fn process_task_streams_output_and_notifies_on_exit() {
    let reg = BackgroundTasks::temporary().unwrap();
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

    let reg = BackgroundTasks::temporary().unwrap();
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

    let reg = BackgroundTasks::temporary().unwrap();
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

    let reg = BackgroundTasks::temporary().unwrap();
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

    let reg = BackgroundTasks::temporary().unwrap();
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
