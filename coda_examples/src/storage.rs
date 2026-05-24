use coda_agent::{AgentCheckpoint, runtime::SessionStorage};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use tokio::fs;

#[derive(Clone, Debug)]
pub struct JsonFileStorage {
    root_dir: PathBuf,
}

impl JsonFileStorage {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
        }
    }

    fn checkpoint_path(&self, thread_id: &str) -> PathBuf {
        self.root_dir.join(format!("thread_{thread_id}.json"))
    }

    fn snapshot_path(&self, session_id: &str) -> PathBuf {
        self.root_dir.join(format!("session_{session_id}.json"))
    }
}

impl SessionStorage for JsonFileStorage {
    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: AgentCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            fs::create_dir_all(&self.root_dir).await.map_err(|err| {
                format!(
                    "failed to create checkpoint directory {}: {err}",
                    self.root_dir.display()
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
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentCheckpoint>, String>> + Send + '_>> {
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
        snapshot: coda_agent::runtime::AgentRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            fs::create_dir_all(&self.root_dir).await.map_err(|err| {
                format!(
                    "failed to create snapshot directory {}: {err}",
                    self.root_dir.display()
                )
            })?;

            let payload = serde_json::to_vec_pretty(&snapshot)
                .map_err(|err| format!("failed to serialize snapshot {session_id}: {err}"))?;
            let path = self.snapshot_path(&session_id);
            fs::write(&path, payload)
                .await
                .map_err(|err| format!("failed to write snapshot {}: {err}", path.display()))?;

            Ok(())
        })
    }

    fn load_session_snapshot(
        &self,
        session_id: &str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<coda_agent::runtime::AgentRuntimeSnapshot>, String>>
                + Send
                + '_,
        >,
    > {
        let path = self.snapshot_path(session_id);
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
