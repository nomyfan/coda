use super::*;

fn owner() -> OutputOwner {
    OutputOwner {
        workspace_id: "workspace".into(),
        session_id: "session".into(),
    }
}

fn store(root: &std::path::Path) -> Store {
    Store::open(OutputLimits {
        root: root.join("output"),
        capture_memory_bytes: 65536,
        result_max_bytes: 1024 * 1024,
        ..OutputLimits::default()
    })
    .unwrap()
}

#[tokio::test]
async fn large_output_has_readable_middle_and_bounded_preview() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let mut capture = store
        .begin(
            owner(),
            vec![Channel::Stdout, Channel::Stderr],
            CapturePurpose::Foreground,
        )
        .await
        .unwrap();
    for _ in 0..20 {
        capture
            .append(Channel::Stdout, &vec![b'a'; IO_BLOCK_BYTES])
            .await;
    }
    capture.append(Channel::Stdout, b"middle-marker").await;
    for _ in 0..20 {
        capture
            .append(Channel::Stdout, &vec![b'z'; IO_BLOCK_BYTES])
            .await;
    }
    capture.append(Channel::Stderr, b"warning").await;
    let output = capture
        .finish(tokio::time::Instant::now() + FINALIZE_TIMEOUT)
        .await;
    let OutputData::Captured(output) = output else {
        panic!("expected spill")
    };
    assert!(output.preview.len() < store.limits().capture_memory_bytes);
    assert!(output.preview.contains("warning"));
    let reference = output.reference.as_ref().unwrap();
    assert!(reference.complete);
    let stdout = std::fs::read(&reference.channels[0].path).unwrap();
    assert!(stdout.windows(13).any(|bytes| bytes == b"middle-marker"));
    assert!(store.charged_bytes() >= stdout.len() as u64 + OBJECT_OVERHEAD);
}

#[tokio::test]
async fn result_limit_keeps_prefix_and_latest_preview() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let mut capture = store
        .begin(owner(), vec![Channel::Stdout], CapturePurpose::Foreground)
        .await
        .unwrap();
    for _ in 0..128 {
        capture
            .append(Channel::Stdout, &vec![b'a'; IO_BLOCK_BYTES])
            .await;
    }
    capture.append(Channel::Stdout, b"tail-marker").await;
    let OutputData::Captured(output) = capture
        .finish(tokio::time::Instant::now() + FINALIZE_TIMEOUT)
        .await
    else {
        panic!()
    };
    assert_eq!(output.failure, Some(StorageFailure::ResultLimit));
    assert!(output.preview.ends_with("tail-marker"));
    let reference = output.reference.unwrap();
    assert!(!reference.complete);
    assert_eq!(
        std::fs::metadata(&reference.channels[0].path)
            .unwrap()
            .len(),
        1024 * 1024
    );
    assert!(matches!(
        output
            .buffer
            .materialize(
                &BufferBudget::new(16 * 1024 * 1024),
                &coda_core::tool::CancellationToken::new()
            )
            .await
            .unwrap_err(),
        OutputError::Incomplete(_)
    ));
}

#[tokio::test]
async fn expired_finalization_does_not_publish_a_path() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let mut capture = store
        .begin(owner(), vec![Channel::Stdout], CapturePurpose::Foreground)
        .await
        .unwrap();
    for _ in 0..8 {
        capture
            .append(Channel::Stdout, &vec![b'x'; IO_BLOCK_BYTES])
            .await;
    }
    let OutputData::Captured(output) = capture
        .finish(tokio::time::Instant::now() - Duration::from_secs(1))
        .await
    else {
        panic!()
    };
    assert!(output.reference.is_none());
    assert_eq!(output.failure, Some(StorageFailure::FinalizeTimeout));
    assert!(!output.preview.is_empty());
}

#[tokio::test]
async fn explicit_retention_spills_values_smaller_than_the_capture_cache() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let OutputData::Captured(output) = store
        .retain(
            owner(),
            "hello".into(),
            tokio::time::Instant::now() + FINALIZE_TIMEOUT,
        )
        .await
    else {
        panic!()
    };
    assert_eq!(
        std::fs::read_to_string(&output.reference.unwrap().channels[0].path).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn programmatic_data_is_exact_and_has_no_history_reference() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let budget = BufferBudget::new(4 * 1024 * 1024);
    let mut capture = store
        .begin(
            owner(),
            vec![Channel::Stdout],
            CapturePurpose::Programmatic(budget.clone()),
        )
        .await
        .unwrap();
    for _ in 0..8 {
        capture
            .append(Channel::Stdout, &vec![b'x'; IO_BLOCK_BYTES])
            .await;
    }
    let output = capture
        .finish(tokio::time::Instant::now() + FINALIZE_TIMEOUT)
        .await;
    assert!(matches!(&output, OutputData::Captured(data) if data.reference.is_none()));
    let result = output
        .materialize(&budget, &coda_core::tool::CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.text, "x".repeat(8 * IO_BLOCK_BYTES));
    assert!(budget.available() < budget.capacity());
    drop(result);
    assert_eq!(budget.available(), budget.capacity());
}

#[tokio::test]
async fn every_seal_barrier_failure_returns_preview_without_a_reference() {
    for step in ["file_sync", "manifest_replace", "directory_sync"] {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let mut capture = store
            .begin(owner(), vec![Channel::Stdout], CapturePurpose::Background)
            .await
            .unwrap();
        capture
            .append(Channel::Stdout, b"retained diagnostic")
            .await;
        *store.inner.hook.lock().unwrap() = Some((step, Box::new(|| Err(StorageFailure::Io))));
        let reader = capture.reader();
        let OutputData::Captured(output) = capture
            .finish(tokio::time::Instant::now() + FINALIZE_TIMEOUT)
            .await
        else {
            panic!()
        };
        assert!(output.reference.is_none(), "{step}");
        assert!(output.preview.contains("retained diagnostic"));
        assert_eq!(reader.snapshot().failure, Some(StorageFailure::Io));
        assert!(reader.read(Channel::Stdout, 0, 100).await.is_err());
    }
}

#[tokio::test]
async fn timed_out_seal_keeps_inflight_bytes_charged_until_io_finishes() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let mut capture = store
        .begin(owner(), vec![Channel::Stdout], CapturePurpose::Background)
        .await
        .unwrap();
    capture
        .append(Channel::Stdout, &vec![b'x'; IO_BLOCK_BYTES])
        .await;
    let (entered, reached) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    *store.inner.hook.lock().unwrap() = Some((
        "directory_sync",
        Box::new(move || {
            let _ = entered.send(());
            blocked.recv().unwrap();
            Ok(())
        }),
    ));
    let before = store.charged_bytes();
    let start = tokio::time::Instant::now();
    let finish = tokio::spawn(capture.finish(start + Duration::from_millis(100)));
    reached.await.unwrap();
    let OutputData::Captured(output) = finish.await.unwrap() else {
        panic!()
    };
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(output.reference.is_none());
    store.inner.cleanup(None, true);
    assert_eq!(store.charged_bytes(), before);
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while store.charged_bytes() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(store.inner.objects.entries().unwrap().count(), 0);
}

#[tokio::test]
async fn shared_quota_evicts_unpinned_outputs_and_never_live_captures() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(OutputLimits {
        root: root.path().join("output"),
        result_max_bytes: 1024 * 1024,
        session_disk_bytes: 1024 * 1024,
        total_disk_bytes: 1024 * 1024,
        ..OutputLimits::default()
    })
    .unwrap();
    let first = store
        .retain(
            owner(),
            "x".repeat(600 * 1024),
            tokio::time::Instant::now() + FINALIZE_TIMEOUT,
        )
        .await;
    let OutputData::Captured(ref captured) = first else {
        panic!()
    };
    let path = captured.reference.as_ref().unwrap().channels[0]
        .path
        .clone();
    let second = store
        .retain(
            owner(),
            "y".repeat(600 * 1024),
            tokio::time::Instant::now() + FINALIZE_TIMEOUT,
        )
        .await;
    assert!(
        matches!(&second, OutputData::Captured(c) if c.failure == Some(StorageFailure::SessionQuota))
    );
    assert!(path.exists());
    drop(first);
    drop(second);
    let third = store
        .retain(
            owner(),
            "z".repeat(600 * 1024),
            tokio::time::Instant::now() + FINALIZE_TIMEOUT,
        )
        .await;
    assert!(matches!(&third, OutputData::Captured(c) if c.reference.as_ref().unwrap().complete));
    assert!(!path.exists());
    assert!(store.charged_bytes() <= 1024 * 1024);
}

#[tokio::test]
async fn failed_deletion_stays_charged_and_recovery_counts_orphan_ownership() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let output = store
        .retain(
            owner(),
            "persisted".into(),
            tokio::time::Instant::now() + FINALIZE_TIMEOUT,
        )
        .await;
    let OutputData::Captured(ref captured) = output else {
        panic!()
    };
    let id = captured.reference.as_ref().unwrap().id;
    let dir = store.inner.objects.open_dir(id.to_string()).unwrap();
    let charged = store.charged_bytes();
    drop(output);
    *store.inner.hook.lock().unwrap() = Some(("delete", Box::new(|| Err(StorageFailure::Io))));
    store.inner.cleanup(None, true);
    assert_eq!(store.charged_bytes(), charged);
    dir.unlink(FileName::Meta).unwrap();
    // Read recovery directly before cleanup to verify the crash-leftover charge.
    store.inner.ledger.lock().unwrap().clear();
    store.inner.recover().unwrap();
    let ledger = store.inner.ledger.lock().unwrap();
    assert_eq!(ledger[&id].owner, owner());
    assert_eq!(ledger[&id].charged, charged);
    drop(ledger);
    store.inner.cleanup(None, false);
    assert_eq!(store.charged_bytes(), 0);
}

#[tokio::test]
async fn history_source_is_archived_once_and_reopens_under_the_same_path() {
    let root = tempfile::tempdir().unwrap();
    let source = coda_core::llm::MessageId::new();
    let limits;
    let reference;
    {
        let store = store(root.path());
        limits = store.limits().clone();
        let output = store
            .retain_source(
                owner(),
                source,
                "history".repeat(4000),
                tokio::time::Instant::now() + FINALIZE_TIMEOUT,
            )
            .await;
        let OutputData::Captured(output) = output else {
            panic!()
        };
        reference = output.reference.unwrap();
        drop(output.buffer);
        let used = store.charged_bytes();
        let again = store
            .retain_source(
                owner(),
                source,
                "history".repeat(4000),
                tokio::time::Instant::now() + FINALIZE_TIMEOUT,
            )
            .await;
        assert!(
            matches!(again, OutputData::Captured(c) if c.reference.as_ref() == Some(&reference))
        );
        assert_eq!(store.charged_bytes(), used);
    }
    // Background cleanup closures may still own the root for a short time.
    let reopened = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(store) = Store::open(limits.clone()) {
                break store;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let again = reopened
        .retain_source(
            owner(),
            source,
            "history".repeat(4000),
            tokio::time::Instant::now() + FINALIZE_TIMEOUT,
        )
        .await;
    assert!(matches!(again, OutputData::Captured(c) if c.reference == Some(reference)));
}
