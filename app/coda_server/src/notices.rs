//! Persistence for undelivered background-task notices.
//!
//! One document per session, written by the hub alone: at entry release it is
//! rewritten to whatever is still undelivered, and at entry initialization it
//! is read back and restored into the fresh registry. Deliberately not part
//! of `StoredRuntimeSnapshot` — that file is rewritten whole by the runtime
//! at several points, and a field the runtime doesn't carry in memory would
//! be zeroed on every such save.
//!
//! Delivery is exactly-once across the normal lifecycle and may lose or
//! duplicate across a server crash — see the design doc's delivery semantics.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use coda_tools::TaskNotice;
use tokio::fs;

use crate::hub::SessionKey;
use crate::storage::WorkspaceStorage;

/// File name inside the session directory. Lives with the rest of the
/// session's persisted state so `delete_session`'s recursive removal cleans
/// it up too.
const PENDING_NOTICES_FILE: &str = "pending_notices.json";

pub trait NoticeStore: Send + Sync {
    /// Pending notices persisted for `key`. A missing document is an empty
    /// list; a corrupt or unreadable one is an error — the caller logs it and
    /// proceeds with no restored notices.
    fn load<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<TaskNotice>, String>> + Send + 'a>>;

    /// Rewrite `key`'s document to exactly `pending` (empty included: release
    /// time is when delivered notices leave the store). On failure the
    /// previous document is left intact — its stale entries dedupe away on
    /// restore, but notices new to this incarnation are lost with it.
    fn save<'a>(
        &'a self,
        key: &'a SessionKey,
        pending: &'a [TaskNotice],
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// File-backed [`NoticeStore`] over the per-workspace session directories.
pub struct FsNoticeStore {
    workspaces: HashMap<String, WorkspaceStorage>,
}

impl FsNoticeStore {
    pub fn new(workspaces: HashMap<String, WorkspaceStorage>) -> Self {
        Self { workspaces }
    }

    fn path(&self, key: &SessionKey) -> Result<std::path::PathBuf, String> {
        let storage = self
            .workspaces
            .get(&key.0)
            .ok_or_else(|| format!("unknown workspace '{}'", key.0))?;
        Ok(storage.session_dir(&key.1)?.join(PENDING_NOTICES_FILE))
    }
}

impl NoticeStore for FsNoticeStore {
    fn load<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<TaskNotice>, String>> + Send + 'a>> {
        Box::pin(async move {
            let path = self.path(key)?;
            let bytes = match fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
            };
            match serde_json::from_slice(&bytes) {
                Ok(notices) => Ok(notices),
                Err(err) => {
                    // Move the corrupt document aside so the next load starts
                    // clean instead of failing forever.
                    let aside = path.with_extension("json.corrupt");
                    let _ = fs::rename(&path, &aside).await;
                    Err(format!(
                        "corrupt pending-notices file {} (moved aside): {err}",
                        path.display()
                    ))
                }
            }
        })
    }

    fn save<'a>(
        &'a self,
        key: &'a SessionKey,
        pending: &'a [TaskNotice],
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let path = self.path(key)?;
            let json = serde_json::to_vec(pending)
                .map_err(|err| format!("failed to serialize pending notices: {err}"))?;
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)
                    .await
                    .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
            }
            // Temp file + rename: a failed write never leaves a half-written
            // document behind, only the previous one.
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, &json)
                .await
                .map_err(|err| format!("failed to write {}: {err}", tmp.display()))?;
            fs::rename(&tmp, &path)
                .await
                .map_err(|err| format!("failed to rename {}: {err}", path.display()))
        })
    }
}

/// In-memory [`NoticeStore`] for hub tests.
#[cfg(test)]
pub(crate) struct MemNoticeStore {
    docs: std::sync::Mutex<HashMap<SessionKey, Vec<TaskNotice>>>,
}

#[cfg(test)]
impl Default for MemNoticeStore {
    fn default() -> Self {
        Self {
            docs: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
impl MemNoticeStore {
    pub(crate) fn get(&self, key: &SessionKey) -> Vec<TaskNotice> {
        self.docs
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn put(&self, key: SessionKey, notices: Vec<TaskNotice>) {
        self.docs.lock().unwrap().insert(key, notices);
    }
}

#[cfg(test)]
impl NoticeStore for MemNoticeStore {
    fn load<'a>(
        &'a self,
        key: &'a SessionKey,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<TaskNotice>, String>> + Send + 'a>> {
        let notices = self.get(key);
        Box::pin(async move { Ok(notices) })
    }

    fn save<'a>(
        &'a self,
        key: &'a SessionKey,
        pending: &'a [TaskNotice],
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        self.put(key.clone(), pending.to_vec());
        Box::pin(async move { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_tools::TaskStatus;

    fn store_at(dir: &std::path::Path) -> FsNoticeStore {
        let mut workspaces = HashMap::new();
        workspaces.insert("ws".to_string(), WorkspaceStorage::new(dir.to_path_buf()));
        FsNoticeStore::new(workspaces)
    }

    fn notice(id: &str) -> TaskNotice {
        TaskNotice::Task {
            id: id.parse().expect("valid task id"),
            command: "cmd".into(),
            description: String::new(),
            status: TaskStatus::Exited {
                code: Some(0),
                at: jiff::Timestamp::now(),
            },
            output_tail: "tail".into(),
            stdout_overwritten: 0,
            stderr_overwritten: 0,
        }
    }

    #[tokio::test]
    async fn save_load_roundtrip_and_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(dir.path());
        let key = ("ws".to_string(), "s1".to_string());
        assert!(store.load(&key).await.unwrap().is_empty());

        let id = "bg_0123456789abcdef0123456789abcdef";
        store.save(&key, &[notice(id)]).await.unwrap();
        let loaded = store.load(&key).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].keys(),
            vec![coda_core::llm::TaskNoticeKey::Completed {
                task_id: id.to_string()
            }]
        );

        // Release-time rewrite to empty clears the document.
        store.save(&key, &[]).await.unwrap();
        assert!(store.load(&key).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn corrupt_file_errors_and_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(dir.path());
        let key = ("ws".to_string(), "s2".to_string());
        let session_dir = dir.path().join("s2");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join(PENDING_NOTICES_FILE), b"{not json").unwrap();

        let err = store.load(&key).await.unwrap_err();
        assert!(err.contains("corrupt"), "unexpected error: {err}");
        // Moved aside: the next load starts clean.
        assert!(store.load(&key).await.unwrap().is_empty());
        assert!(session_dir.join("pending_notices.json.corrupt").exists());
    }

    #[tokio::test]
    async fn unsafe_session_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(dir.path());
        let key = ("ws".to_string(), "..".to_string());
        assert!(store.load(&key).await.is_err());
        assert!(store.save(&key, &[]).await.is_err());
    }
}
