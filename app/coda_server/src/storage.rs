use coda_agent::HistoryEntry;
use coda_agent::agent::ReplyTarget;
use coda_agent::persist::{StoredCheckpoint, StoredResumePoint, StoredRuntimeSnapshot};
use coda_agent::runtime::SessionStorage;
use coda_core::llm::{Message, MessageId, TurnId};
use coda_tools::TodoItem;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Json;
use std::collections::HashSet;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::fs;
use tokio::sync::Mutex;

/// Connect to PostgreSQL and bring the schema up to date.
///
/// Migrations are embedded in the binary and run on every start, so deploying
/// is all it takes to create or update the schema. The error deliberately omits
/// the URL, which carries the password.
pub async fn connect(database_url: &str) -> Result<PgPool, String> {
    let pool = PgPoolOptions::new()
        .connect(database_url)
        .await
        .map_err(|err| format!("failed to connect to the database: {err}"))?;
    sqlx::migrate!()
        .run(&pool)
        .await
        .map_err(|err| format!("failed to apply database migrations: {err}"))?;
    Ok(pool)
}

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

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct SessionModelBinding {
    pub provider_id: String,
    pub model_id: String,
    pub reasoning_effort: Option<String>,
}

impl SessionModelBinding {
    pub fn selection_key(&self) -> String {
        format!("{}:{}", self.provider_id, self.model_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct SessionMetadata {
    pub name: Option<String>,
    pub binding: SessionModelBinding,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitializedSession {
    pub metadata: SessionMetadata,
    pub created: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionMetadataError {
    InvalidSessionId(String),
    SessionNotFound,
    BindingMismatch,
    Persistence(String),
}

impl std::fmt::Display for SessionMetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSessionId(message) => write!(f, "{message}"),
            Self::SessionNotFound => write!(f, "session not found"),
            Self::BindingMismatch => write!(f, "session model binding does not match"),
            Self::Persistence(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for SessionMetadataError {}

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
    pub async fn initialize_session(
        &self,
        session_id: &str,
        requested_binding: SessionModelBinding,
    ) -> Result<InitializedSession, SessionMetadataError> {
        validate_session_id(session_id).map_err(SessionMetadataError::InvalidSessionId)?;
        let _guard = self.metadata_write_lock.lock().await;
        let dir = self.session_dir(session_id);
        fs::create_dir_all(&dir).await.map_err(|err| {
            SessionMetadataError::Persistence(format!(
                "failed to create session directory {}: {err}",
                dir.display()
            ))
        })?;

        let path = self.metadata_path(session_id);
        match Self::read_metadata(&path).await {
            Ok(metadata) => Ok(InitializedSession {
                metadata,
                created: false,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let metadata = SessionMetadata {
                    name: None,
                    binding: requested_binding,
                };
                Self::write_metadata(&path, &metadata)
                    .await
                    .map_err(SessionMetadataError::Persistence)?;
                Ok(InitializedSession {
                    metadata,
                    created: true,
                })
            }
            Err(err) => Err(SessionMetadataError::Persistence(format!(
                "failed to initialize session metadata {}: {err}",
                path.display()
            ))),
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
        let mut metadata = Self::read_metadata(&path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                RenameSessionError::SessionNotFound
            } else {
                RenameSessionError::Persistence(format!(
                    "failed to read session metadata {}: {err}",
                    path.display()
                ))
            }
        })?;
        metadata.name = name.clone();
        Self::write_metadata(&path, &metadata)
            .await
            .map_err(RenameSessionError::Persistence)?;
        Ok(name)
    }

    pub async fn update_reasoning_effort(
        &self,
        session_id: &str,
        expected_provider_id: &str,
        expected_model_id: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<SessionModelBinding, SessionMetadataError> {
        validate_session_id(session_id).map_err(SessionMetadataError::InvalidSessionId)?;
        let _guard = self.metadata_write_lock.lock().await;
        let path = self.metadata_path(session_id);
        let mut metadata = Self::read_metadata(&path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                SessionMetadataError::SessionNotFound
            } else {
                SessionMetadataError::Persistence(format!(
                    "failed to read session metadata {}: {err}",
                    path.display()
                ))
            }
        })?;
        if metadata.binding.provider_id != expected_provider_id
            || metadata.binding.model_id != expected_model_id
        {
            return Err(SessionMetadataError::BindingMismatch);
        }
        metadata.binding.reasoning_effort = reasoning_effort.map(str::to_string);
        Self::write_metadata(&path, &metadata)
            .await
            .map_err(SessionMetadataError::Persistence)?;
        Ok(metadata.binding)
    }

    async fn read_metadata(path: &Path) -> Result<SessionMetadata, std::io::Error> {
        let payload = fs::read(path).await?;
        serde_json::from_slice(&payload).map_err(std::io::Error::other)
    }

    async fn write_metadata(path: &Path, metadata: &SessionMetadata) -> Result<(), String> {
        let payload = serde_json::to_vec_pretty(metadata)
            .map_err(|err| format!("failed to serialize session metadata: {err}"))?;
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let mut file = atomic_write_file::AtomicWriteFile::open(&path).map_err(|err| {
                format!("failed to open session metadata {}: {err}", path.display())
            })?;
            file.write_all(&payload).map_err(|err| {
                format!("failed to write session metadata {}: {err}", path.display())
            })?;
            file.commit().map_err(|err| {
                format!(
                    "failed to commit session metadata {}: {err}",
                    path.display()
                )
            })
        })
        .await
        .map_err(|err| format!("session metadata writer task failed: {err}"))?
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
            .find_map(|entry| match entry.message {
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

/// Persistence for one session, backed by PostgreSQL.
///
/// Messages are rows, so saving a checkpoint appends only what the thread has
/// gained since the last save instead of rewriting its whole history. The
/// starting point is the thread's own `message_count`: it is both "how many
/// messages are already stored" and "the next free `seq`", which holds because
/// `seq` is exactly the index into the checkpoint's message vector.
#[derive(Clone, Debug)]
pub struct PgSessionStorage {
    pool: PgPool,
    workspace_id: String,
    session_id: String,
}

/// A thread's state, minus its conversation.
#[derive(sqlx::FromRow)]
struct CheckpointRow {
    agent_name: String,
    parent_thread_id: Option<String>,
    derivation_key: Option<String>,
    reply_target: Option<Json<ReplyTarget>>,
    resume_point: Json<StoredResumePoint>,
    todos: Json<Vec<TodoItem>>,
    suspended_at: chrono::DateTime<chrono::Utc>,
}

#[derive(sqlx::FromRow)]
struct MessageRow {
    turn_id: uuid::Uuid,
    payload: Json<Message>,
}

/// The identity and role columns of a message row.
///
/// A `System` message never reaches a checkpoint: the system prompt is inserted
/// on a clone when a request is built, and `restore_history` drops it. So there
/// is no row shape for one, and hitting this means the invariant broke upstream —
/// worth a failed save rather than a minted id nobody can trace.
fn message_row_identity(message: &Message) -> Result<(MessageId, &'static str), String> {
    Ok(match message {
        Message::User(message) => (message.message_id, "user"),
        Message::Assistant(message) => (message.message_id, "assistant"),
        Message::Tool(message) => (message.message_id, "tool"),
        Message::System(_) => {
            return Err("cannot persist a system message: the system prompt is not history".into());
        }
    })
}

/// Whether a thread in this state is waiting on a human.
fn awaits_approval(resume_point: &StoredResumePoint) -> bool {
    matches!(
        resume_point,
        StoredResumePoint::PendingApproval {
            pending_approval_calls,
            ..
        } if !pending_approval_calls.is_empty()
    )
}

/// jiff timestamps cross the SQL boundary as chrono values, which is what sqlx
/// binds to `timestamptz`. Both directions go through microseconds — PostgreSQL's
/// own resolution — so the value that comes back is exactly the one written,
/// truncated once on the way in rather than rounded by the server.
fn sql_timestamp(timestamp: jiff::Timestamp) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_micros(timestamp.as_microsecond())
        .expect("jiff's timestamp range is a subset of chrono's")
}

fn jiff_timestamp(timestamp: chrono::DateTime<chrono::Utc>) -> Result<jiff::Timestamp, String> {
    jiff::Timestamp::from_microsecond(timestamp.timestamp_micros())
        .map_err(|err| format!("stored timestamp is out of range: {err}"))
}

impl PgSessionStorage {
    pub fn new(
        pool: PgPool,
        workspace_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            workspace_id: workspace_id.into(),
            session_id: session_id.into(),
        }
    }

    /// Append the thread's new messages and overwrite its state, atomically.
    async fn write_checkpoint(
        &self,
        thread_id: &str,
        checkpoint: StoredCheckpoint,
    ) -> Result<(), String> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|err| format!("failed to open a transaction for {thread_id}: {err}"))?;

        let stored_count: Option<i32> = sqlx::query_scalar(
            "select message_count from thread_checkpoints
              where workspace_id = $1 and session_id = $2 and thread_id = $3",
        )
        .bind(&self.workspace_id)
        .bind(&self.session_id)
        .bind(thread_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| format!("failed to read the message count of {thread_id}: {err}"))?;
        let stored_count = stored_count.unwrap_or(0) as usize;

        // History is append-only, which is what makes appending the tail
        // equivalent to rewriting the thread. A shorter checkpoint means someone
        // rewrote history without resetting the count, and appending from the old
        // count would drop messages silently.
        if checkpoint.messages.len() < stored_count {
            return Err(format!(
                "thread {thread_id} has {stored_count} stored messages but the checkpoint carries \
                 {}; message history is append-only",
                checkpoint.messages.len()
            ));
        }

        for (offset, entry) in checkpoint.messages[stored_count..].iter().enumerate() {
            let (message_id, role) = message_row_identity(&entry.message)?;
            let origin = match &entry.message {
                Message::User(message) => message.origin.as_ref(),
                _ => None,
            };
            sqlx::query(
                "insert into messages
                    (workspace_id, session_id, thread_id, seq, message_id, turn_id, role,
                     origin_message_id, origin_call_id, payload)
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            )
            .bind(&self.workspace_id)
            .bind(&self.session_id)
            .bind(thread_id)
            .bind((stored_count + offset) as i32)
            .bind(message_id.as_uuid())
            .bind(entry.turn_id.as_uuid())
            .bind(role)
            .bind(origin.map(|origin| origin.message_id.as_uuid()))
            .bind(origin.map(|origin| origin.call_id.as_str()))
            .bind(Json(&entry.message))
            .execute(&mut *tx)
            .await
            .map_err(|err| {
                format!(
                    "failed to append message {} of {thread_id}: {err}",
                    stored_count + offset
                )
            })?;
        }

        sqlx::query(
            "insert into thread_checkpoints
                (workspace_id, session_id, thread_id, agent_name, parent_thread_id,
                 derivation_key, reply_target, resume_point, todos, suspended_at,
                 message_count, pending_approval)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             on conflict (workspace_id, session_id, thread_id) do update set
                agent_name = excluded.agent_name,
                parent_thread_id = excluded.parent_thread_id,
                derivation_key = excluded.derivation_key,
                reply_target = excluded.reply_target,
                resume_point = excluded.resume_point,
                todos = excluded.todos,
                suspended_at = excluded.suspended_at,
                message_count = excluded.message_count,
                pending_approval = excluded.pending_approval",
        )
        .bind(&self.workspace_id)
        .bind(&self.session_id)
        .bind(thread_id)
        .bind(&checkpoint.agent_name)
        .bind(&checkpoint.parent_thread_id)
        .bind(&checkpoint.derivation_key)
        .bind(checkpoint.reply_target.as_ref().map(Json))
        .bind(Json(&checkpoint.resume_point))
        .bind(Json(&checkpoint.todos))
        .bind(sql_timestamp(checkpoint.suspended_at))
        .bind(checkpoint.messages.len() as i32)
        .bind(awaits_approval(&checkpoint.resume_point))
        .execute(&mut *tx)
        .await
        .map_err(|err| format!("failed to save the state of {thread_id}: {err}"))?;

        self.touch(&mut tx).await?;
        tx.commit()
            .await
            .map_err(|err| format!("failed to commit the checkpoint of {thread_id}: {err}"))
    }

    async fn read_checkpoint(&self, thread_id: &str) -> Result<Option<StoredCheckpoint>, String> {
        let Some(state) = sqlx::query_as::<_, CheckpointRow>(
            "select agent_name, parent_thread_id, derivation_key, reply_target, resume_point,
                    todos, suspended_at
               from thread_checkpoints
              where workspace_id = $1 and session_id = $2 and thread_id = $3",
        )
        .bind(&self.workspace_id)
        .bind(&self.session_id)
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| format!("failed to load the state of {thread_id}: {err}"))?
        else {
            return Ok(None);
        };

        let messages = sqlx::query_as::<_, MessageRow>(
            "select turn_id, payload from messages
              where workspace_id = $1 and session_id = $2 and thread_id = $3
              order by seq",
        )
        .bind(&self.workspace_id)
        .bind(&self.session_id)
        .bind(thread_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| format!("failed to load the messages of {thread_id}: {err}"))?
        .into_iter()
        .map(|row| HistoryEntry {
            turn_id: TurnId::from(MessageId::from(row.turn_id)),
            message: row.payload.0,
        })
        .collect();

        Ok(Some(StoredCheckpoint {
            thread_id: thread_id.to_string(),
            agent_name: state.agent_name,
            parent_thread_id: state.parent_thread_id,
            derivation_key: state.derivation_key,
            reply_target: state.reply_target.map(|target| target.0),
            messages,
            todos: state.todos.0,
            resume_point: state.resume_point.0,
            suspended_at: jiff_timestamp(state.suspended_at)?,
        }))
    }

    /// Record that the session changed, so the session list orders by it.
    async fn touch(&self, tx: &mut sqlx::PgTransaction<'_>) -> Result<(), String> {
        sqlx::query(
            "update sessions set updated_at = now() where workspace_id = $1 and session_id = $2",
        )
        .bind(&self.workspace_id)
        .bind(&self.session_id)
        .execute(&mut **tx)
        .await
        .map(|_| ())
        .map_err(|err| {
            format!(
                "failed to mark session {} as updated: {err}",
                self.session_id
            )
        })
    }
}

impl SessionStorage for PgSessionStorage {
    fn save_checkpoint(
        &self,
        thread_id: String,
        checkpoint: StoredCheckpoint,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move { self.write_checkpoint(&thread_id, checkpoint).await })
    }

    fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredCheckpoint>, String>> + Send + '_>> {
        let thread_id = thread_id.to_string();
        Box::pin(async move { self.read_checkpoint(&thread_id).await })
    }

    fn save_session_snapshot(
        &self,
        _session_id: String,
        snapshot: StoredRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let mut tx =
                self.pool.begin().await.map_err(|err| {
                    format!("failed to open a transaction for the snapshot: {err}")
                })?;
            sqlx::query(
                "insert into runtime_snapshots (workspace_id, session_id, snapshot)
                 values ($1, $2, $3)
                 on conflict (workspace_id, session_id) do update set
                    snapshot = excluded.snapshot",
            )
            .bind(&self.workspace_id)
            .bind(&self.session_id)
            .bind(Json(&snapshot))
            .execute(&mut *tx)
            .await
            .map_err(|err| format!("failed to save the runtime snapshot: {err}"))?;
            self.touch(&mut tx).await?;
            tx.commit()
                .await
                .map_err(|err| format!("failed to commit the runtime snapshot: {err}"))
        })
    }

    fn load_session_snapshot(
        &self,
        _session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredRuntimeSnapshot>, String>> + Send + '_>>
    {
        Box::pin(async move {
            sqlx::query_scalar::<_, Json<StoredRuntimeSnapshot>>(
                "select snapshot from runtime_snapshots
                  where workspace_id = $1 and session_id = $2",
            )
            .bind(&self.workspace_id)
            .bind(&self.session_id)
            .fetch_optional(&self.pool)
            .await
            .map(|snapshot| snapshot.map(|snapshot| snapshot.0))
            .map_err(|err| format!("failed to load the runtime snapshot: {err}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_agent::HistoryEntry;
    use coda_agent::persist::StoredResumePoint;
    use coda_core::llm::{
        AssistantMessage, MessageId, ReasoningContinuation, ToolCall, TurnId, UserMessage,
    };

    /// Wrap a message as a history entry. These tests exercise storage of the
    /// conversation, not turn grouping, so each gets its own turn.
    fn entry(message: Message) -> HistoryEntry {
        HistoryEntry {
            turn_id: TurnId::from(MessageId::new()),
            message,
        }
    }
    use std::os::unix::fs::PermissionsExt as _;

    fn test_binding() -> SessionModelBinding {
        SessionModelBinding {
            provider_id: "openrouter".into(),
            model_id: "x-ai/grok-4.5".into(),
            reasoning_effort: Some("high".into()),
        }
    }

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

        storage
            .initialize_session("active", test_binding())
            .await
            .unwrap();
        fs::write(active.snapshot_path(), b"{}").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        storage
            .initialize_session("other", test_binding())
            .await
            .unwrap();
        fs::write(other.snapshot_path(), b"{}").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        active
            .save_checkpoint(
                "active".into(),
                StoredCheckpoint {
                    thread_id: "active".into(),
                    agent_name: "coda".into(),
                    parent_thread_id: None,
                    derivation_key: None,
                    reply_target: None,
                    messages: vec![entry(Message::User(UserMessage::text(
                        MessageId::new(),
                        "recent session",
                    )))],
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
        storage
            .initialize_session("images", test_binding())
            .await
            .unwrap();

        session
            .save_checkpoint(
                "images".into(),
                StoredCheckpoint {
                    thread_id: "images".into(),
                    agent_name: "coda".into(),
                    parent_thread_id: None,
                    derivation_key: None,
                    reply_target: None,
                    messages: vec![entry(Message::User(UserMessage::with_images(
                        MessageId::new(),
                        "",
                        &["data:image/png;base64,AAAA".to_string()],
                    )))],
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
    async fn checkpoint_round_trips_reasoning_continuation() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = WorkspaceStorage::new(workspace.path().join("sessions"));
        let session = storage.session("continuation");
        storage
            .initialize_session("continuation", test_binding())
            .await
            .unwrap();
        let now = jiff::Timestamp::now();
        session
            .save_checkpoint(
                "continuation".into(),
                StoredCheckpoint {
                    thread_id: "continuation".into(),
                    agent_name: "coda".into(),
                    parent_thread_id: None,
                    derivation_key: None,
                    reply_target: None,
                    messages: vec![entry(Message::Assistant(AssistantMessage {
                        message_id: MessageId::new(),
                        content: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "call_weather".into(),
                            name: "lookup_weather".into(),
                            arguments: Some(r#"{"city":"Singapore"}"#.into()),
                        }],
                        usage: None,
                        reasoning_content: Some("Need current weather.".into()),
                        reasoning_continuation: Some(
                            ReasoningContinuation::try_new(
                                "openrouter.reasoning_details.v1",
                                serde_json::json!([
                                    {"type": "reasoning.text", "text": "Need current weather."},
                                    {"type": "reasoning.encrypted", "data": "opaque"}
                                ]),
                            )
                            .unwrap(),
                        ),
                        reasoning_ended_at: Some(now),
                        aborted: false,
                        started_at: now,
                        ended_at: now,
                    }))],
                    todos: vec![],
                    resume_point: StoredResumePoint::Generation,
                    suspended_at: now,
                },
            )
            .await
            .unwrap();

        let checkpoint = session
            .load_checkpoint("continuation")
            .await
            .unwrap()
            .unwrap();
        let Message::Assistant(message) = &checkpoint.messages[0].message else {
            panic!("expected assistant message");
        };
        let continuation = message
            .reasoning_continuation
            .as_ref()
            .expect("reasoning continuation was not restored");
        assert_eq!(continuation.format(), "openrouter.reasoning_details.v1");
        assert_eq!(
            continuation.payload_for("openrouter.reasoning_details.v1"),
            Some(&serde_json::json!([
                {"type": "reasoning.text", "text": "Need current weather."},
                {"type": "reasoning.encrypted", "data": "opaque"}
            ]))
        );
    }

    #[tokio::test]
    async fn list_sessions_marks_pending_approval() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = WorkspaceStorage::new(workspace.path().join("sessions"));
        let session = storage.session("review");
        storage
            .initialize_session("review", test_binding())
            .await
            .unwrap();

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
                        parent_thread_id: None,
                        derivation_key: None,
                        reply_target: None,
                        messages: vec![],
                        todos: vec![],
                        resume_point: StoredResumePoint::PendingApproval {
                            parent_message_id: MessageId::new(),
                            pending_approval_calls: vec![coda_core::llm::ToolCall {
                                id: format!("{thread_id}-call"),
                                name: "shell".into(),
                                arguments: Some(r#"{"command":"cargo test"}"#.into()),
                            }],
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
        let initialized = storage
            .initialize_session("session-1", test_binding())
            .await
            .unwrap();
        assert!(initialized.created);
        assert_eq!(initialized.metadata.binding, test_binding());

        let reopened = storage
            .initialize_session(
                "session-1",
                SessionModelBinding {
                    provider_id: "other".into(),
                    model_id: "different".into(),
                    reasoning_effort: None,
                },
            )
            .await
            .unwrap();
        assert!(!reopened.created);
        assert_eq!(reopened.metadata.binding, test_binding());

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
        let metadata = WorkspaceStorage::read_metadata(&storage.metadata_path("session-1"))
            .await
            .unwrap();
        assert_eq!(metadata.binding, test_binding());

        let binding = storage
            .update_reasoning_effort("session-1", "openrouter", "x-ai/grok-4.5", Some("low"))
            .await
            .unwrap();
        assert_eq!(binding.reasoning_effort.as_deref(), Some("low"));
        let metadata = WorkspaceStorage::read_metadata(&storage.metadata_path("session-1"))
            .await
            .unwrap();
        assert_eq!(metadata.name.as_deref(), Some("Investigation"));

        assert_eq!(
            storage
                .rename_session("session-1", Some(" "))
                .await
                .unwrap(),
            None
        );
        assert_eq!(storage.list_sessions().await.unwrap()[0].name, None);
        let metadata = WorkspaceStorage::read_metadata(&storage.metadata_path("session-1"))
            .await
            .unwrap();
        assert_eq!(metadata.binding.reasoning_effort.as_deref(), Some("low"));
    }

    #[tokio::test]
    async fn effort_update_rejects_a_different_model_binding() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = WorkspaceStorage::new(workspace.path().join("sessions"));
        storage
            .initialize_session("session-1", test_binding())
            .await
            .unwrap();

        assert_eq!(
            storage
                .update_reasoning_effort(
                    "session-1",
                    "openrouter",
                    "moonshotai/kimi-k3",
                    Some("low"),
                )
                .await,
            Err(SessionMetadataError::BindingMismatch)
        );
        assert_eq!(
            WorkspaceStorage::read_metadata(&storage.metadata_path("session-1"))
                .await
                .unwrap()
                .binding,
            test_binding()
        );
    }

    #[tokio::test]
    async fn failed_atomic_metadata_write_preserves_the_previous_file() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = WorkspaceStorage::new(workspace.path().join("sessions"));
        storage
            .initialize_session("session-1", test_binding())
            .await
            .unwrap();
        let session_dir = storage.session_dir("session-1");
        let original = fs::read(storage.metadata_path("session-1")).await.unwrap();

        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = storage
            .update_reasoning_effort("session-1", "openrouter", "x-ai/grok-4.5", Some("low"))
            .await;
        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(result, Err(SessionMetadataError::Persistence(_))));
        assert_eq!(
            fs::read(storage.metadata_path("session-1")).await.unwrap(),
            original
        );
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
        storage
            .initialize_session("valid", test_binding())
            .await
            .unwrap();

        let sessions = storage.list_sessions().await.unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "valid");
    }
}
