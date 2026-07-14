use coda_agent::persist::{StoredCheckpoint, StoredResumePoint, StoredRuntimeSnapshot};
use coda_agent::runtime::SessionStorage;
use coda_core::llm::Message;
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Session-list preview shown for a session whose first user turn carried only
/// images (no text). Kept in sync with `IMAGE_ONLY_TITLE` in the web store so the
/// optimistic title and the persisted one match.
const IMAGE_ONLY_PREVIEW: &str = "[image]";

/// Reject session IDs that are unsafe to use as a path component.
///
/// `session_id` is client-controlled and gets joined under the workspace's
/// session root to read, write, and delete files. A value containing path
/// separators or `..` would escape that root (directory traversal → arbitrary
/// file overwrite or recursive deletion), so callers must validate before any
/// filesystem use. A single component that is not `.`/`..` and contains no
/// separator or NUL byte cannot escape its parent directory.
pub fn validate_session_id(session_id: &str) -> Result<(), String> {
    let unsafe_id = session_id.is_empty()
        || session_id == "."
        || session_id == ".."
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains('\0');
    if unsafe_id {
        return Err(format!("invalid session id: {session_id:?}"));
    }
    Ok(())
}

/// Persistence for all sessions of a single workspace. Each session lives in its
/// own subdirectory (`<root>/<session_id>/`) holding the runtime snapshot and the
/// per-thread checkpoints.
#[derive(Clone, Debug)]
pub struct WorkspaceStorage {
    root_dir: PathBuf,
    metadata_write_lock: Arc<Mutex<()>>,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct SessionMetadata {
    name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameSessionError {
    InvalidSessionId(String),
    InvalidName(String),
    SessionNotFound,
    Persistence(String),
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SessionFile {
    pub session_id: String,
    pub name: Option<String>,
    pub updated_at_ms: Option<u64>,
    pub first_user_message: Option<String>,
    pub has_pending_approval: bool,
}

fn normalize_session_name(
    requested_name: Option<&str>,
) -> Result<Option<String>, RenameSessionError> {
    let Some(name) = requested_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return Ok(None);
    };
    if name.chars().count() > 120 {
        return Err(RenameSessionError::InvalidName(
            "session name must be at most 120 characters".to_string(),
        ));
    }
    if name
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
    {
        return Err(RenameSessionError::InvalidName(
            "session name must be a single line without control characters".to_string(),
        ));
    }
    Ok(Some(name.to_string()))
}

impl WorkspaceStorage {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
            metadata_write_lock: Arc::new(Mutex::new(())),
        }
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root_dir.join(session_id)
    }

    fn metadata_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("metadata.json")
    }

    /// Create the durable identity record for a newly opened session.
    pub async fn initialize_session(&self, session_id: &str) -> Result<(), String> {
        validate_session_id(session_id)?;
        let _guard = self.metadata_write_lock.lock().await;
        let dir = self.session_dir(session_id);
        fs::create_dir_all(&dir).await.map_err(|err| {
            format!(
                "failed to create session directory {}: {err}",
                dir.display()
            )
        })?;

        let path = self.metadata_path(session_id);
        match Self::read_metadata(&path).await {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Self::write_metadata(&path, &SessionMetadata::default()).await
            }
            Err(err) => Err(format!(
                "failed to initialize session metadata {}: {err}",
                path.display()
            )),
        }
    }

    pub async fn rename_session(
        &self,
        session_id: &str,
        requested_name: Option<&str>,
    ) -> Result<Option<String>, RenameSessionError> {
        validate_session_id(session_id).map_err(RenameSessionError::InvalidSessionId)?;
        let name = normalize_session_name(requested_name)?;
        let _guard = self.metadata_write_lock.lock().await;
        let path = self.metadata_path(session_id);
        Self::read_metadata(&path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                RenameSessionError::SessionNotFound
            } else {
                RenameSessionError::Persistence(format!(
                    "failed to read session metadata {}: {err}",
                    path.display()
                ))
            }
        })?;
        Self::write_metadata(&path, &SessionMetadata { name: name.clone() })
            .await
            .map_err(RenameSessionError::Persistence)?;
        Ok(name)
    }

    async fn read_metadata(path: &Path) -> Result<SessionMetadata, std::io::Error> {
        let payload = fs::read(path).await?;
        serde_json::from_slice(&payload).map_err(std::io::Error::other)
    }

    async fn write_metadata(path: &Path, metadata: &SessionMetadata) -> Result<(), String> {
        let payload = serde_json::to_vec_pretty(metadata)
            .map_err(|err| format!("failed to serialize session metadata: {err}"))?;
        let temp_path =
            path.with_file_name(format!(".metadata-{}.tmp", Uuid::new_v4().as_hyphenated()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .await
            .map_err(|err| {
                format!(
                    "failed to create temporary metadata file {}: {err}",
                    temp_path.display()
                )
            })?;
        let result = async {
            file.write_all(&payload).await?;
            file.sync_all().await?;
            drop(file);
            fs::rename(&temp_path, path).await
        }
        .await;
        if let Err(err) = result {
            let _ = fs::remove_file(&temp_path).await;
            return Err(format!(
                "failed to write session metadata {}: {err}",
                path.display()
            ));
        }
        Ok(())
    }

    /// Storage scoped to one session's directory.
    pub fn session(&self, session_id: &str) -> JsonFileStorage {
        JsonFileStorage::new(self.root_dir.join(session_id))
    }

    /// Remove a session's directory and everything in it.
    pub async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        validate_session_id(session_id)?;
        let dir = self.root_dir.join(session_id);
        match fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!("failed to delete session {}: {err}", dir.display())),
        }
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionFile>, String> {
        let mut dir = match fs::read_dir(&self.root_dir).await {
            Ok(dir) => dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(format!(
                    "failed to read session directory {}: {err}",
                    self.root_dir.display()
                ));
            }
        };

        let mut sessions = Vec::new();
        while let Some(entry) = dir.next_entry().await.map_err(|err| {
            format!(
                "failed to read session directory {}: {err}",
                self.root_dir.display()
            )
        })? {
            if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let file_name = entry.file_name();
            let Some(session_id) = file_name.to_str() else {
                continue;
            };
            let metadata_path = self.metadata_path(session_id);
            let metadata = match Self::read_metadata(&metadata_path).await {
                Ok(metadata) => metadata,
                Err(err) => {
                    tracing::warn!(
                        session_id,
                        path = %metadata_path.display(),
                        "skipping session with invalid metadata: {err}"
                    );
                    continue;
                }
            };
            let storage = self.session(session_id);
            let updated_at_ms = fs::metadata(storage.checkpoint_path(session_id))
                .await
                .or(fs::metadata(storage.snapshot_path()).await)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .and_then(|duration| duration.as_millis().try_into().ok());
            let first_user_message = storage.first_user_message(session_id).await;
            let has_pending_approval = storage.has_pending_approval(session_id).await;
            sessions.push(SessionFile {
                session_id: session_id.to_string(),
                name: metadata.name,
                updated_at_ms,
                first_user_message,
                has_pending_approval,
            });
        }

        sessions.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(sessions)
    }
}

/// Persistence scoped to a single session directory.
#[derive(Clone, Debug)]
pub struct JsonFileStorage {
    dir: PathBuf,
}

impl JsonFileStorage {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn checkpoint_path(&self, thread_id: &str) -> PathBuf {
        self.dir.join(format!("thread_{thread_id}.json"))
    }

    fn snapshot_path(&self) -> PathBuf {
        self.dir.join("snapshot.json")
    }

    async fn first_user_message(&self, session_id: &str) -> Option<String> {
        let checkpoint = self.load_checkpoint(session_id).await.ok().flatten()?;
        checkpoint
            .messages
            .into_iter()
            .find_map(|message| match message {
                Message::User(msg) => Some(msg),
                _ => None,
            })
            .and_then(|msg| match msg.first_text() {
                Some(text) => Some(text.to_string()),
                // An image-only first turn has no text; show a placeholder so the
                // session list doesn't fall back to the raw session id. Keep this
                // string in sync with `IMAGE_ONLY_TITLE` in the web store.
                None if msg.has_image() => Some(IMAGE_ONLY_PREVIEW.to_string()),
                None => None,
            })
    }

    async fn has_pending_approval(&self, session_id: &str) -> bool {
        let mut seen = HashSet::from([session_id.to_string()]);
        let mut thread_ids = vec![session_id.to_string()];

        if let Some(snapshot) = self.load_session_snapshot(session_id).await.ok().flatten() {
            for thread_id in snapshot.active_threads.into_values() {
                if seen.insert(thread_id.clone()) {
                    thread_ids.push(thread_id);
                }
            }
        }

        for thread_id in thread_ids {
            if self.checkpoint_has_pending_approval(&thread_id).await {
                return true;
            }
        }
        false
    }

    async fn checkpoint_has_pending_approval(&self, thread_id: &str) -> bool {
        self.load_checkpoint(thread_id)
            .await
            .ok()
            .flatten()
            .is_some_and(|checkpoint| {
                matches!(
                    checkpoint.resume_point,
                    StoredResumePoint::PendingApproval {
                        pending_approval_calls,
                        ..
                    } if !pending_approval_calls.is_empty()
                )
            })
    }
}

impl SessionStorage for JsonFileStorage {
    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            fs::create_dir_all(&self.dir).await.map_err(|err| {
                format!(
                    "failed to create checkpoint directory {}: {err}",
                    self.dir.display()
                )
            })?;

            let payload = serde_json::to_vec_pretty(&checkpoint)
                .map_err(|err| format!("failed to serialize checkpoint {thread_id}: {err}"))?;
            let path = self.checkpoint_path(&thread_id);
            fs::write(&path, payload)
                .await
                .map_err(|err| format!("failed to write checkpoint {}: {err}", path.display()))?;

            Ok(())
        })
    }

    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredCheckpoint>, String>> + Send + '_>> {
        let path = self.checkpoint_path(thread_id);
        Box::pin(async move {
            let payload = match fs::read(&path).await {
                Ok(payload) => payload,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(err) => {
                    return Err(format!(
                        "failed to read checkpoint {}: {err}",
                        path.display()
                    ));
                }
            };

            serde_json::from_slice(&payload)
                .map(Some)
                .map_err(|err| format!("failed to parse checkpoint {}: {err}", path.display()))
        })
    }

    fn save_session_snapshot(
        &self,
        session_id: String,
        snapshot: StoredRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            fs::create_dir_all(&self.dir).await.map_err(|err| {
                format!(
                    "failed to create snapshot directory {}: {err}",
                    self.dir.display()
                )
            })?;

            let payload = serde_json::to_vec_pretty(&snapshot)
                .map_err(|err| format!("failed to serialize snapshot {session_id}: {err}"))?;
            let path = self.snapshot_path();
            fs::write(&path, payload)
                .await
                .map_err(|err| format!("failed to write snapshot {}: {err}", path.display()))?;

            Ok(())
        })
    }

    fn load_session_snapshot(
        &self,
        _session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredRuntimeSnapshot>, String>> + Send + '_>>
    {
        let path = self.snapshot_path();
        Box::pin(async move {
            let payload = match fs::read(&path).await {
                Ok(payload) => payload,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(err) => {
                    return Err(format!("failed to read snapshot {}: {err}", path.display()));
                }
            };
            serde_json::from_slice(&payload)
                .map(Some)
                .map_err(|err| format!("failed to parse snapshot {}: {err}", path.display()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_agent::persist::StoredResumePoint;
    use coda_core::llm::UserMessage;

    #[test]
    fn validate_session_id_accepts_uuid_like_ids() {
        assert!(validate_session_id("3c4e75c-abcd-1234").is_ok());
        assert!(validate_session_id("session_42").is_ok());
    }

    #[test]
    fn validate_session_id_rejects_traversal_and_separators() {
        for bad in ["", ".", "..", "../escape", "a/b", "a\\b", "x\0y"] {
            assert!(
                validate_session_id(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[tokio::test]
    async fn delete_session_rejects_traversal_without_touching_filesystem() {
        let workspace = tempfile::tempdir().unwrap();
        let sentinel = workspace.path().join("keep.txt");
        std::fs::write(&sentinel, b"important").unwrap();

        let storage = WorkspaceStorage::new(workspace.path().join("sessions"));
        // `..` would resolve to the workspace dir; the guard must reject it
        // before `remove_dir_all` runs.
        assert!(storage.delete_session("..").await.is_err());
        assert!(sentinel.exists(), "traversal must not delete outside root");
    }

    #[tokio::test]
    async fn list_sessions_uses_root_checkpoint_for_recent_activity() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions_dir = workspace.path().join("sessions");
        let storage = WorkspaceStorage::new(&sessions_dir);
        let active = storage.session("active");
        let other = storage.session("other");

        storage.initialize_session("active").await.unwrap();
        fs::write(active.snapshot_path(), b"{}").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        storage.initialize_session("other").await.unwrap();
        fs::write(other.snapshot_path(), b"{}").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        active
            .save_checkpoint(
                "active".into(),
                StoredCheckpoint {
                    thread_id: "active".into(),
                    agent_name: "coda".into(),
                    reply_target: None,
                    messages: vec![Message::User(UserMessage::text("recent session"))],
                    todos: vec![],
                    resume_point: StoredResumePoint::Generation,
                    suspended_at: jiff::Timestamp::default(),
                },
            )
            .await
            .unwrap();

        let sessions = storage.list_sessions().await.unwrap();

        assert_eq!(sessions[0].session_id, "active");
        assert!(sessions[0].updated_at_ms > sessions[1].updated_at_ms);
        assert_eq!(
            sessions[0].first_user_message.as_deref(),
            Some("recent session")
        );
        assert!(!sessions[0].has_pending_approval);
    }

    #[tokio::test]
    async fn first_user_message_previews_image_only_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = WorkspaceStorage::new(workspace.path().join("sessions"));
        let session = storage.session("images");
        storage.initialize_session("images").await.unwrap();

        session
            .save_checkpoint(
                "images".into(),
                StoredCheckpoint {
                    thread_id: "images".into(),
                    agent_name: "coda".into(),
                    reply_target: None,
                    messages: vec![Message::User(UserMessage::with_images(
                        "",
                        &["data:image/png;base64,AAAA".to_string()],
                    ))],
                    todos: vec![],
                    resume_point: StoredResumePoint::Generation,
                    suspended_at: jiff::Timestamp::default(),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            session.first_user_message("images").await.as_deref(),
            Some(IMAGE_ONLY_PREVIEW)
        );
    }

    #[tokio::test]
    async fn list_sessions_marks_pending_approval() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = WorkspaceStorage::new(workspace.path().join("sessions"));
        let session = storage.session("review");
        storage.initialize_session("review").await.unwrap();

        session
            .save_session_snapshot(
                "review".into(),
                StoredRuntimeSnapshot {
                    drained_envelopes: Default::default(),
                    agent_drained_envelopes: Default::default(),
                    active_threads: [("sub".to_string(), "sub-thread".to_string())].into(),
                },
            )
            .await
            .unwrap();
        for thread_id in ["review", "sub-thread"] {
            session
                .save_checkpoint(
                    thread_id.into(),
                    StoredCheckpoint {
                        thread_id: thread_id.into(),
                        agent_name: "coda".into(),
                        reply_target: None,
                        messages: vec![],
                        todos: vec![],
                        resume_point: StoredResumePoint::PendingApproval {
                            pending_approval_calls: vec![coda_core::llm::ToolCall {
                                id: format!("{thread_id}-call"),
                                name: "shell".into(),
                                arguments: Some(r#"{"command":"cargo test"}"#.into()),
                            }]
                            .into(),
                            pending_calls: vec![],
                        },
                        suspended_at: jiff::Timestamp::default(),
                    },
                )
                .await
                .unwrap();
        }

        let sessions = storage.list_sessions().await.unwrap();

        assert_eq!(sessions[0].session_id, "review");
        assert!(sessions[0].has_pending_approval);
    }

    #[test]
    fn session_name_normalization_validates_length_and_controls() {
        assert_eq!(
            normalize_session_name(Some("  研究会话  ")).unwrap(),
            Some("研究会话".to_string())
        );
        assert_eq!(normalize_session_name(Some("  ")).unwrap(), None);
        assert!(normalize_session_name(Some(&"名".repeat(120))).is_ok());
        assert!(matches!(
            normalize_session_name(Some(&"名".repeat(121))),
            Err(RenameSessionError::InvalidName(_))
        ));
        for invalid in [
            "line\nbreak",
            "line\rbreak",
            "nul\0byte",
            "line\u{2028}break",
        ] {
            assert!(matches!(
                normalize_session_name(Some(invalid)),
                Err(RenameSessionError::InvalidName(_))
            ));
        }
    }

    #[tokio::test]
    async fn session_metadata_initializes_renames_and_clears() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = WorkspaceStorage::new(workspace.path().join("sessions"));
        storage.initialize_session("session-1").await.unwrap();

        assert_eq!(
            storage
                .rename_session("session-1", Some("  Investigation  "))
                .await
                .unwrap(),
            Some("Investigation".to_string())
        );
        assert_eq!(
            storage.list_sessions().await.unwrap()[0].name.as_deref(),
            Some("Investigation")
        );

        assert_eq!(
            storage
                .rename_session("session-1", Some(" "))
                .await
                .unwrap(),
            None
        );
        assert_eq!(storage.list_sessions().await.unwrap()[0].name, None);
    }

    #[tokio::test]
    async fn rename_does_not_create_a_missing_session() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions_dir = workspace.path().join("sessions");
        let storage = WorkspaceStorage::new(&sessions_dir);

        assert_eq!(
            storage.rename_session("missing", Some("name")).await,
            Err(RenameSessionError::SessionNotFound)
        );
        assert!(!sessions_dir.join("missing").exists());
    }

    #[tokio::test]
    async fn list_sessions_skips_missing_or_invalid_metadata() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions_dir = workspace.path().join("sessions");
        let storage = WorkspaceStorage::new(&sessions_dir);
        fs::create_dir_all(sessions_dir.join("missing"))
            .await
            .unwrap();
        fs::create_dir_all(sessions_dir.join("invalid"))
            .await
            .unwrap();
        fs::write(sessions_dir.join("invalid/metadata.json"), b"not json")
            .await
            .unwrap();
        storage.initialize_session("valid").await.unwrap();

        let sessions = storage.list_sessions().await.unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "valid");
    }
}
