use super::*;
use crate::{BackgroundTasks, TaskArchive, TaskExit, TaskKind, TaskMeta, TaskOrigin};

fn contents(path: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(contents(&path));
        } else {
            files.insert(path.clone(), std::fs::read(path).unwrap());
        }
    }
    files
}

#[tokio::test]
async fn archived_reads_preserve_results_cursors_and_notices() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let registry = BackgroundTasks::session_backed(root).await.unwrap();
    let shell = registry
        .spawn_with(
            TaskMeta::shell("test".into(), "output".into(), "coda".into()),
            |ctx| async move {
                ctx.append_stdout(b"saved output").await.unwrap();
                ctx.append_stderr(b"warning").await.unwrap();
                TaskExit::Exited { code: Some(0) }
            },
        )
        .await
        .unwrap();
    registry.wait_terminal(&shell).await;
    let agent = registry
        .spawn_identified(
            TaskId::new(),
            TaskMeta {
                kind: TaskKind::Subagent {
                    agent_name: "removed-agent".into(),
                },
                description: "result".into(),
                parent_task_id: None,
                origin: TaskOrigin::default(),
            },
            |_| async {
                TaskExit::Completed {
                    answer: "saved answer".into(),
                }
            },
        )
        .await
        .unwrap();
    registry.wait_terminal(&agent).await;
    registry.shutdown().await;
    drop(registry);
    let before = contents(tmp.path());
    let archive = ArchivedTasks::open_existing(tmp.path()).unwrap().unwrap();
    assert_eq!(archive.overview().await.unwrap().len(), 2);
    for _ in 0..2 {
        let Some(TaskResult::Available {
            output: TaskResultOutput::Shell { stdout, stderr, .. },
            ..
        }) = archive.read_result(&shell).await.unwrap()
        else {
            panic!("shell output")
        };
        assert_eq!(stdout, "saved output");
        assert_eq!(stderr, "warning");
        let Some(TaskResult::Available {
            output: TaskResultOutput::Subagent { answer },
            ..
        }) = archive.read_result(&agent).await.unwrap()
        else {
            panic!("agent output")
        };
        assert_eq!(answer, "saved answer");
    }
    assert_eq!(before, contents(tmp.path()));
}

#[tokio::test]
async fn unfinished_archive_is_neither_recovered_nor_cleaned() {
    let tmp = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(tmp.path()).unwrap();
    let archive = TaskArchive::new(root);
    let id = TaskId::new();
    let record = archive
        .create_unreserved(
            &id,
            &TaskMeta {
                kind: TaskKind::Subagent {
                    agent_name: "worker".into(),
                },
                description: "unfinished".into(),
                parent_task_id: None,
                origin: TaskOrigin::default(),
            },
        )
        .await
        .unwrap();
    record.write_result("not committed".into()).await.unwrap();
    drop(record);
    drop(archive);
    let before = contents(tmp.path());
    let archive = ArchivedTasks::open_existing(tmp.path()).unwrap().unwrap();
    let overview = archive.overview().await.unwrap();
    assert!(matches!(overview[0].status, TaskStatus::Running));
    assert!(!overview[0].subtree_active);
    assert!(!overview[0].result_available);
    assert!(matches!(
        archive.read_result(&id).await.unwrap(),
        Some(TaskResult::Pending { .. })
    ));
    assert_eq!(before, contents(tmp.path()));
}

#[test]
fn missing_and_symlink_archives_are_not_created_or_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing");
    assert!(ArchivedTasks::open_existing(&missing).unwrap().is_none());
    assert!(!missing.exists());
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(tmp.path(), &link).unwrap();
    assert!(ArchivedTasks::open_existing(&link).is_err());
}

#[tokio::test]
async fn corrupt_expired_and_unknown_results_remain_distinct_without_cleanup() {
    use crate::manifest::{ExpireReason, OutputDisposition, TaskOutputManifest};
    let tmp = tempfile::tempdir().unwrap();
    let registry =
        BackgroundTasks::session_backed(ArchiveDir::open_or_create_root(tmp.path()).unwrap())
            .await
            .unwrap();
    let id = registry
        .spawn_identified(
            TaskId::new(),
            TaskMeta {
                kind: TaskKind::Subagent {
                    agent_name: "worker".into(),
                },
                description: "result".into(),
                parent_task_id: None,
                origin: TaskOrigin::default(),
            },
            |_| async {
                TaskExit::Completed {
                    answer: "answer".into(),
                }
            },
        )
        .await
        .unwrap();
    registry.wait_terminal(&id).await;
    registry.shutdown().await;
    drop(registry);
    let archive = ArchivedTasks::open_existing(tmp.path()).unwrap().unwrap();
    assert!(archive.read_result(&TaskId::new()).await.unwrap().is_none());
    let task_dir = tmp.path().join(id.as_str());
    std::fs::write(task_dir.join("result.txt"), b"bad length").unwrap();
    let before = contents(tmp.path());
    assert!(archive.read_result(&id).await.is_err());
    assert_eq!(before, contents(tmp.path()));
    let manifest_file = task_dir.join("meta.json");
    let mut manifest: TaskOutputManifest =
        serde_json::from_slice(&std::fs::read(&manifest_file).unwrap()).unwrap();
    manifest.output = OutputDisposition::Expired {
        at: jiff::Timestamp::now(),
        reason: ExpireReason::SessionQuota,
    };
    std::fs::write(manifest_file, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let before = contents(tmp.path());
    assert!(matches!(
        archive.read_result(&id).await.unwrap(),
        Some(TaskResult::Expired { .. })
    ));
    assert!(!archive.overview().await.unwrap()[0].result_available);
    assert_eq!(before, contents(tmp.path()));
}
