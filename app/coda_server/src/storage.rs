use coda_agent::persist::{StoredCheckpoint, StoredRuntimeSnapshot};
use coda_agent::runtime::SessionStorage;
use coda_core::llm::Message;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::UNIX_EPOCH;
use tokio::fs;

/// Persistence for all sessions of a single workspace. Each session lives in its
/// own subdirectory (`<root>/<session_id>/`) holding the runtime snapshot and the
/// per-thread checkpoints.
#[derive(Clone, Debug)]
pub struct WorkspaceStorage {
    root_dir: PathBuf,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SessionFile {
    pub session_id: String,
    pub updated_at_ms: Option<u64>,
    pub first_user_message: Option<String>,
}

impl WorkspaceStorage {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
        }
    }

    /// Storage scoped to one session's directory.
    pub fn session(&self, session_id: &str) -> JsonFileStorage {
        JsonFileStorage::new(self.root_dir.join(session_id))
    }

    /// Remove a session's directory and everything in it.
    pub async fn delete_session(&self, session_id: &str) -> Result<(), String> {
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
            let storage = self.session(session_id);
            let updated_at_ms = fs::metadata(storage.snapshot_path())
                .await
                .or(entry.metadata().await)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .and_then(|duration| duration.as_millis().try_into().ok());
            let first_user_message = storage.first_user_message(session_id).await;
            sessions.push(SessionFile {
                session_id: session_id.to_string(),
                updated_at_ms,
                first_user_message,
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
                Message::User(message) => Some(message.0),
                _ => None,
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
