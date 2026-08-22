use super::*;
use coda_core::tool::{HostCallScope, HostEffectLimits};

/// A registry per test: none of these exercise sharing, and isolation keeps
/// them from queueing behind each other.
fn test_locks() -> Arc<KeyedLock<String>> {
    Arc::new(KeyedLock::new())
}

fn tmp_file(name: &str, content: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("coda_edit_test_{}_{}", std::process::id(), name));
    std::fs::write(&path, content).unwrap();
    path
}

/// Two edits to one file, dispatched together the way the runtime
/// dispatches a batch of tool calls. Unlocked, both read the original and
/// the second write drops the first edit — silently, since both calls
/// report success.
#[tokio::test]
async fn concurrent_edits_to_one_file_both_land() {
    let path = tmp_file("concurrent_small", "alpha\nbeta\n");
    let file_path = path.to_str().unwrap().to_string();
    let tool = EditFileTool::new(test_locks());

    let first = tool.execute(
        EditFileToolParams {
            file_path: file_path.clone(),
            old_string: "alpha".to_string(),
            new_string: "ALPHA".to_string(),
            replace_all: None,
        },
        ToolCallContext::default(),
    );
    let second = tool.execute(
        EditFileToolParams {
            file_path,
            old_string: "beta".to_string(),
            new_string: "BETA".to_string(),
            replace_all: None,
        },
        ToolCallContext::default(),
    );
    let (first, second) = tokio::join!(first, second);

    assert!(first.is_ok() && second.is_ok(), "{first:?} {second:?}");
    let content = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(content, "ALPHA\nBETA\n");
}

/// The same race on a file big enough that the writer yields mid-write,
/// when an in-place truncate leaves the file empty or half written.
#[tokio::test]
async fn concurrent_edits_to_one_large_file_both_land() {
    let mut body = String::from("HEAD\n");
    for i in 0..200_000 {
        body.push_str(&format!("line {i}\n"));
    }
    body.push_str("TAIL\n");
    let path = tmp_file("concurrent_large", &body);
    let file_path = path.to_str().unwrap().to_string();
    let tool = EditFileTool::new(test_locks());

    let first = tool.execute(
        EditFileToolParams {
            file_path: file_path.clone(),
            old_string: "HEAD".to_string(),
            new_string: "HEAD-EDITED".to_string(),
            replace_all: None,
        },
        ToolCallContext::default(),
    );
    let second = tool.execute(
        EditFileToolParams {
            file_path,
            old_string: "TAIL".to_string(),
            new_string: "TAIL-EDITED".to_string(),
            replace_all: None,
        },
        ToolCallContext::default(),
    );
    let (first, second) = tokio::join!(first, second);

    assert!(first.is_ok() && second.is_ok(), "{first:?} {second:?}");
    let content = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(content.len(), body.len() + "-EDITED".len() * 2);
    assert!(content.starts_with("HEAD-EDITED\n"));
    assert!(content.ends_with("TAIL-EDITED\n"));
}

/// Sub-agents get their own tool instances; only the shared registry makes
/// their edits exclude each other.
#[tokio::test]
async fn separate_tool_instances_sharing_a_registry_exclude_each_other() {
    let path = tmp_file("concurrent_instances", "alpha\nbeta\n");
    let file_path = path.to_str().unwrap().to_string();
    let locks = test_locks();

    let first = EditFileTool::new(locks.clone()).execute(
        EditFileToolParams {
            file_path: file_path.clone(),
            old_string: "alpha".to_string(),
            new_string: "ALPHA".to_string(),
            replace_all: None,
        },
        ToolCallContext::default(),
    );
    let second = EditFileTool::new(locks).execute(
        EditFileToolParams {
            file_path,
            old_string: "beta".to_string(),
            new_string: "BETA".to_string(),
            replace_all: None,
        },
        ToolCallContext::default(),
    );
    let (first, second) = tokio::join!(first, second);

    assert!(first.is_ok() && second.is_ok(), "{first:?} {second:?}");
    let content = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(content, "ALPHA\nBETA\n");
}

/// Two spellings of one path must map to one key, or they lock different
/// entries and race anyway.
#[tokio::test]
async fn lock_key_is_path_shape_independent() {
    let path = tmp_file("lock_key", "x\n");
    let indirect = path.parent().unwrap().join("..").join(
        path.parent()
            .unwrap()
            .file_name()
            .map(std::path::PathBuf::from)
            .unwrap()
            .join(path.file_name().unwrap()),
    );
    let direct_key = resolve_lock_key(&path).await.unwrap();
    let indirect_key = resolve_lock_key(&indirect).await.unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(direct_key, indirect_key);
}

/// The cancellation context must reach the child-process runner: a token
/// cancelled up front settles as Aborted instead of running fd.
#[tokio::test]
async fn ls_pre_cancelled_context_aborts() {
    let ctx = ToolCallContext::default();
    ctx.cancel.cancel();
    let result = ListDirectoryTool::new()
        .execute(
            ListDirectoryToolParams {
                path: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            ctx,
        )
        .await;
    assert!(matches!(result, Err(ToolError::Aborted(_))));
}

#[tokio::test]
async fn edit_replaces_unique_match() {
    let path = tmp_file("unique", "hello world\nfoo bar\n");
    let tool = EditFileTool::new(test_locks());
    let result = tool
        .execute(
            EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "foo bar".to_string(),
                new_string: "baz qux".to_string(),
                replace_all: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap();
    assert!(result.contains("1 occurrence"));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "hello world\nbaz qux\n"
    );
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn edit_errors_when_not_found() {
    let path = tmp_file("notfound", "hello world\n");
    let tool = EditFileTool::new(test_locks());
    let err = tool
        .execute(
            EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "missing".to_string(),
                new_string: "x".to_string(),
                replace_all: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn edit_errors_on_ambiguous_match() {
    let path = tmp_file("ambiguous", "x\nx\n");
    let tool = EditFileTool::new(test_locks());
    let err = tool
        .execute(
            EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "x".to_string(),
                new_string: "y".to_string(),
                replace_all: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn edit_replace_all() {
    let path = tmp_file("all", "x\nx\nx\n");
    let tool = EditFileTool::new(test_locks());
    let result = tool
        .execute(
            EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "x".to_string(),
                new_string: "y".to_string(),
                replace_all: Some(true),
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap();
    assert!(result.contains("3 occurrence"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\ny\ny\n");
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn edit_errors_on_identical_strings() {
    let path = tmp_file("identical", "x\n");
    let tool = EditFileTool::new(test_locks());
    let err = tool
        .execute(
            EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "x".to_string(),
                new_string: "x".to_string(),
                replace_all: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn edit_requires_absolute_path() {
    let tool = EditFileTool::new(test_locks());
    let err = tool
        .execute(
            EditFileToolParams {
                file_path: "relative.txt".to_string(),
                old_string: "a".to_string(),
                new_string: "b".to_string(),
                replace_all: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
}

fn tmp_path(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("coda_fs_test_{}_{}", std::process::id(), name));
    path
}

fn tmp_huge_file(name: &str) -> std::path::PathBuf {
    let path = tmp_path(name);
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_FILE_SIZE + 1).unwrap();
    path
}

#[tokio::test]
async fn write_refuses_huge_file() {
    let path = tmp_path("huge_write");
    std::fs::remove_file(&path).ok();
    let tool = WriteFileTool::new(test_locks());
    let err = tool
        .execute(
            WriteFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                content: "a".repeat((MAX_FILE_SIZE + 1) as usize),
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    assert!(!path.exists());
}

#[tokio::test]
async fn read_refuses_huge_file() {
    let path = tmp_huge_file("huge_read");
    let tool = ReadFileTool::new();
    let err = tool
        .execute(
            ReadFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                offset: None,
                limit: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn edit_refuses_huge_file() {
    let path = tmp_huge_file("huge_edit");
    let tool = EditFileTool::new(test_locks());
    let err = tool
        .execute(
            EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "a".to_string(),
                new_string: "b".to_string(),
                replace_all: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn read_decodes_invalid_utf8_lossily() {
    let path = tmp_path("lossy_read");
    std::fs::write(&path, b"before \xFF\xFE after\n").unwrap();
    let tool = ReadFileTool::new();
    let result = tool
        .execute(
            ReadFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                offset: None,
                limit: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap();
    assert!(result.contains("before \u{FFFD}\u{FFFD} after"));
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn edit_refuses_invalid_utf8() {
    let path = tmp_path("non_utf8_edit");
    std::fs::write(&path, b"before \xFF\xFE after\n").unwrap();
    let tool = EditFileTool::new(test_locks());
    let err = tool
        .execute(
            EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "before".to_string(),
                new_string: "changed".to_string(),
                replace_all: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    assert_eq!(std::fs::read(&path).unwrap(), b"before \xFF\xFE after\n");
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn write_creates_new_file() {
    let path = tmp_path("write_new");
    std::fs::remove_file(&path).ok();
    let tool = WriteFileTool::new(test_locks());
    let ctx = ToolCallContext::default();
    tool.execute(
        WriteFileToolParams {
            file_path: path.to_str().unwrap().to_string(),
            content: "hello".to_string(),
        },
        ctx.clone(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    let artifacts = ctx.take_artifacts();
    let [
        ToolArtifact::FileDiff {
            path: artifact_path,
            operation,
            patch,
        },
    ] = artifacts.as_slice()
    else {
        panic!("expected one file diff artifact");
    };
    assert_eq!(artifact_path, path.to_str().unwrap());
    assert_eq!(*operation, FileChangeOperation::Create);
    assert!(
        patch.starts_with(&format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n",
            path = path.display(),
        )),
        "{patch}",
    );
    assert!(patch.contains("+hello"), "{patch}");
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn write_artifact_budget_failure_does_not_create_parent_or_file() {
    let parent = tmp_path("write_budget_parent");
    std::fs::remove_dir_all(&parent).ok();
    let path = parent.join("nested/file.txt");
    let scope = HostCallScope::new(
        ToolCallContext::default(),
        HostEffectLimits {
            state_bytes: 1024,
            artifact_bytes: 1,
        },
    );
    let child = scope.begin_tool_call(Default::default());
    let error = WriteFileTool::new(test_locks())
        .execute(
            WriteFileToolParams {
                file_path: path.to_string_lossy().into_owned(),
                content: "hello".to_string(),
            },
            child.context(),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ToolError::ResourceLimit(_)));
    assert!(!parent.exists());
}

#[tokio::test]
async fn edit_artifact_budget_failure_leaves_file_unchanged() {
    let path = tmp_file("edit_budget", "before\n");
    let scope = HostCallScope::new(
        ToolCallContext::default(),
        HostEffectLimits {
            state_bytes: 1024,
            artifact_bytes: 1,
        },
    );
    let child = scope.begin_tool_call(Default::default());
    let error = EditFileTool::new(test_locks())
        .execute(
            EditFileToolParams {
                file_path: path.to_string_lossy().into_owned(),
                old_string: "before".to_string(),
                new_string: "after".to_string(),
                replace_all: None,
            },
            child.context(),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ToolError::ResourceLimit(_)));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");
    std::fs::remove_file(path).ok();
}

#[tokio::test]
async fn edit_records_the_applied_change_as_a_diff_artifact() {
    let path = tmp_file("edit_artifact", "before\n");
    let tool = EditFileTool::new(test_locks());
    let ctx = ToolCallContext::default();
    tool.execute(
        EditFileToolParams {
            file_path: path.to_str().unwrap().to_string(),
            old_string: "before".to_string(),
            new_string: "after".to_string(),
            replace_all: None,
        },
        ctx.clone(),
    )
    .await
    .unwrap();

    let artifacts = ctx.take_artifacts();
    let [
        ToolArtifact::FileDiff {
            operation, patch, ..
        },
    ] = artifacts.as_slice()
    else {
        panic!("expected one file diff artifact");
    };
    assert_eq!(*operation, FileChangeOperation::Modify);
    assert!(
        patch.starts_with(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n",
            path = path.display(),
        )),
        "{patch}",
    );
    assert!(patch.contains("-before"), "{patch}");
    assert!(patch.contains("+after"), "{patch}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn diff_artifact_quotes_special_characters_in_paths() {
    let ToolArtifact::FileDiff { patch, .. } = file_diff_artifact(
        "/tmp/a b\tline\n\"file\\.txt",
        FileChangeOperation::Modify,
        "before\n",
        "after\n",
    );

    assert_eq!(
        patch.lines().next().unwrap(),
        "diff --git \"a//tmp/a b\\tline\\n\\\"file\\\\.txt\" \"b//tmp/a b\\tline\\n\\\"file\\\\.txt\"",
    );
}

#[tokio::test]
async fn write_refuses_existing_file() {
    let path = tmp_file("write_existing", "original\n");
    let tool = WriteFileTool::new(test_locks());
    let err = tool
        .execute(
            WriteFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                content: "clobbered".to_string(),
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn write_refuses_existing_symlink() {
    let target = tmp_file("symlink_write_target", "content\n");
    let link = tmp_path("symlink_write_link");
    std::fs::remove_file(&link).ok();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let tool = WriteFileTool::new(test_locks());
    let err = tool
        .execute(
            WriteFileToolParams {
                file_path: link.to_str().unwrap().to_string(),
                content: "clobbered".to_string(),
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "content\n");
    std::fs::remove_file(&link).ok();
    std::fs::remove_file(&target).ok();
}

#[tokio::test]
async fn read_refuses_symlink() {
    let target = tmp_file("symlink_read_target", "content\n");
    let link = tmp_path("symlink_read_link");
    std::fs::remove_file(&link).ok();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let tool = ReadFileTool::new();
    let err = tool
        .execute(
            ReadFileToolParams {
                file_path: link.to_str().unwrap().to_string(),
                offset: None,
                limit: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    std::fs::remove_file(&link).ok();
    std::fs::remove_file(&target).ok();
}

#[tokio::test]
async fn read_refuses_directory() {
    let tool = ReadFileTool::new();
    let err = tool
        .execute(
            ReadFileToolParams {
                file_path: std::env::temp_dir().to_str().unwrap().to_string(),
                offset: None,
                limit: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
}

#[tokio::test]
async fn edit_refuses_symlink() {
    let target = tmp_file("symlink_edit_target", "content\n");
    let link = tmp_path("symlink_edit_link");
    std::fs::remove_file(&link).ok();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let tool = EditFileTool::new(test_locks());
    let err = tool
        .execute(
            EditFileToolParams {
                file_path: link.to_str().unwrap().to_string(),
                old_string: "content".to_string(),
                new_string: "changed".to_string(),
                replace_all: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "content\n");
    std::fs::remove_file(&link).ok();
    std::fs::remove_file(&target).ok();
}

#[tokio::test]
async fn edit_errors_on_empty_old_string() {
    let path = tmp_file("empty_old", "hello\n");
    let tool = EditFileTool::new(test_locks());
    let err = tool
        .execute(
            EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "".to_string(),
                new_string: "x".to_string(),
                replace_all: None,
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParameters(_)));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
    std::fs::remove_file(&path).ok();
}
