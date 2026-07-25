use coda_agent::HistoryEntry;
use coda_agent::agent::ReplyTarget;
use coda_agent::persist::{StoredCheckpoint, StoredResumePoint, StoredRuntimeSnapshot};
use coda_agent::runtime::SessionStorage;
use coda_core::llm::{Message, MessageId, TurnId};
use coda_tools::TodoItem;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Json;
use std::future::Future;
use std::pin::Pin;

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

/// Reject session IDs that cannot name a session.
///
/// `session_id` is client-controlled. Injection is not the concern — every
/// statement binds it as a parameter — but the rejected set is kept exactly as it
/// was so the contract clients see does not change, and PostgreSQL's `text` will
/// not accept a NUL byte anyway.
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

/// Persistence for all sessions of one workspace.
///
/// Every table is keyed on `(workspace_id, session_id)`, so scoping to a
/// workspace is a bind parameter rather than a directory.
#[derive(Clone, Debug)]
pub struct WorkspaceStorage {
    pool: PgPool,
    workspace_id: String,
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

/// One row of the session list.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub name: Option<String>,
    pub updated_at_ms: u64,
    pub first_user_message: Option<String>,
    pub has_pending_approval: bool,
}

#[derive(sqlx::FromRow)]
struct SessionSummaryRow {
    session_id: String,
    name: Option<String>,
    updated_at_ms: i64,
    has_pending_approval: bool,
    first_user_message: Option<Json<Message>>,
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

/// What the session list shows for a session's first user turn.
fn session_preview(message: &Message) -> Option<String> {
    let Message::User(message) = message else {
        return None;
    };
    match message.first_text() {
        Some(text) => Some(text.to_string()),
        // An image-only first turn has no text; show a placeholder so the
        // session list doesn't fall back to the raw session id. Keep this
        // string in sync with `IMAGE_ONLY_TITLE` in the web store.
        None if message.has_image() => Some(IMAGE_ONLY_PREVIEW.to_string()),
        None => None,
    }
}

impl WorkspaceStorage {
    pub fn new(pool: PgPool, workspace_id: impl Into<String>) -> Self {
        Self {
            pool,
            workspace_id: workspace_id.into(),
        }
    }

    /// Create the durable identity record for a newly opened session, and return
    /// the model binding it will run on — the one stored when it was first
    /// opened, which is what makes a session's model stick across reopens
    /// regardless of what the browser currently prefers.
    pub async fn initialize_session(
        &self,
        session_id: &str,
        requested_binding: SessionModelBinding,
    ) -> Result<SessionModelBinding, SessionMetadataError> {
        validate_session_id(session_id).map_err(SessionMetadataError::InvalidSessionId)?;
        sqlx::query(
            "insert into sessions (workspace_id, session_id, model_binding)
             values ($1, $2, $3)
             on conflict (workspace_id, session_id) do nothing",
        )
        .bind(&self.workspace_id)
        .bind(session_id)
        .bind(Json(&requested_binding))
        .execute(&self.pool)
        .await
        .map_err(|err| {
            SessionMetadataError::Persistence(format!("failed to open session {session_id}: {err}"))
        })?;

        sqlx::query_scalar::<_, Json<SessionModelBinding>>(
            "select model_binding from sessions where workspace_id = $1 and session_id = $2",
        )
        .bind(&self.workspace_id)
        .bind(session_id)
        .fetch_one(&self.pool)
        .await
        .map(|binding| binding.0)
        .map_err(|err| {
            SessionMetadataError::Persistence(format!(
                "failed to read the binding of session {session_id}: {err}"
            ))
        })
    }

    pub async fn rename_session(
        &self,
        session_id: &str,
        requested_name: Option<&str>,
    ) -> Result<Option<String>, RenameSessionError> {
        validate_session_id(session_id).map_err(RenameSessionError::InvalidSessionId)?;
        let name = normalize_session_name(requested_name)?;
        let renamed = sqlx::query(
            "update sessions set name = $3 where workspace_id = $1 and session_id = $2",
        )
        .bind(&self.workspace_id)
        .bind(session_id)
        .bind(name.as_deref())
        .execute(&self.pool)
        .await
        .map_err(|err| {
            RenameSessionError::Persistence(format!("failed to rename session {session_id}: {err}"))
        })?;
        if renamed.rows_affected() == 0 {
            return Err(RenameSessionError::SessionNotFound);
        }
        Ok(name)
    }

    /// Change a session's reasoning effort, but only while it is still on the
    /// model the caller thinks it is. The `where` clause is the compare-and-set:
    /// no row updated means either the session is gone or its binding moved on.
    pub async fn update_reasoning_effort(
        &self,
        session_id: &str,
        expected_provider_id: &str,
        expected_model_id: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<SessionModelBinding, SessionMetadataError> {
        validate_session_id(session_id).map_err(SessionMetadataError::InvalidSessionId)?;
        let effort = reasoning_effort
            .map(|effort| serde_json::Value::String(effort.to_string()))
            .unwrap_or(serde_json::Value::Null);
        let updated = sqlx::query_scalar::<_, Json<SessionModelBinding>>(
            "update sessions
                set model_binding = jsonb_set(model_binding, '{reasoning_effort}', $5)
              where workspace_id = $1 and session_id = $2
                and model_binding->>'provider_id' = $3
                and model_binding->>'model_id' = $4
             returning model_binding",
        )
        .bind(&self.workspace_id)
        .bind(session_id)
        .bind(expected_provider_id)
        .bind(expected_model_id)
        .bind(Json(effort))
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| {
            SessionMetadataError::Persistence(format!(
                "failed to update the reasoning effort of session {session_id}: {err}"
            ))
        })?;

        match updated {
            Some(binding) => Ok(binding.0),
            None if self.session_exists(session_id).await? => {
                Err(SessionMetadataError::BindingMismatch)
            }
            None => Err(SessionMetadataError::SessionNotFound),
        }
    }

    async fn session_exists(&self, session_id: &str) -> Result<bool, SessionMetadataError> {
        sqlx::query_scalar::<_, bool>(
            "select exists(select 1 from sessions where workspace_id = $1 and session_id = $2)",
        )
        .bind(&self.workspace_id)
        .bind(session_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|err| {
            SessionMetadataError::Persistence(format!(
                "failed to look up session {session_id}: {err}"
            ))
        })
    }

    /// Storage scoped to one session.
    pub fn session(&self, session_id: &str) -> PgSessionStorage {
        PgSessionStorage::new(self.pool.clone(), &self.workspace_id, session_id)
    }

    /// Delete a session and everything under it. The threads, messages and
    /// runtime snapshot go with it through `on delete cascade`.
    pub async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        validate_session_id(session_id)?;
        sqlx::query("delete from sessions where workspace_id = $1 and session_id = $2")
            .bind(&self.workspace_id)
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|err| format!("failed to delete session {session_id}: {err}"))
    }

    /// The workspace's sessions, most recently active first.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, String> {
        sqlx::query_as::<_, SessionSummaryRow>(
            "select s.session_id,
                    s.name,
                    (extract(epoch from s.updated_at) * 1000)::int8 as updated_at_ms,
                    exists(select 1 from thread_checkpoints t
                            where t.workspace_id = s.workspace_id
                              and t.session_id = s.session_id
                              and t.pending_approval) as has_pending_approval,
                    (select m.payload from messages m
                      where m.workspace_id = s.workspace_id
                        and m.session_id = s.session_id
                        and m.thread_id = s.session_id
                        and m.role = 'user'
                      order by m.seq
                      limit 1) as first_user_message
               from sessions s
              where s.workspace_id = $1
              order by s.updated_at desc, s.session_id asc",
        )
        .bind(&self.workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| format!("failed to list sessions: {err}"))
        .map(|rows| {
            rows.into_iter()
                .map(|row| SessionSummary {
                    session_id: row.session_id,
                    name: row.name,
                    updated_at_ms: row.updated_at_ms as u64,
                    first_user_message: row
                        .first_user_message
                        .and_then(|message| session_preview(&message.0)),
                    has_pending_approval: row.has_pending_approval,
                })
                .collect()
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

    // The database-backed tests live in `tests/storage_pg.rs`, behind the
    // `pg-tests` feature. These two need no database, so they stay here and run
    // with a plain `cargo test`.

    #[test]
    fn validate_session_id_accepts_uuid_like_ids() {
        assert!(validate_session_id("3c4e75c-abcd-1234").is_ok());
        assert!(validate_session_id("session_42").is_ok());
    }

    #[test]
    fn validate_session_id_rejects_empty_and_separators() {
        for bad in ["", ".", "..", "../escape", "a/b", "a\\b", "x\0y"] {
            assert!(
                validate_session_id(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
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
}
