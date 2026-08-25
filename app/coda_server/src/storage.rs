use crate::jsonb::Json;
use crate::schema::{messages, runtime_snapshots, sessions, thread_checkpoints};
use coda_agent::HistoryEntry;
use coda_agent::ThreadId;
use coda_agent::agent::{ReplyTarget, ThreadStateMap};
use coda_agent::persist::{StoredCheckpoint, StoredResumePoint, StoredRuntimeSnapshot};
use coda_agent::runtime::SessionStorage;
use coda_core::llm::{Message, MessageId, TurnId};
use diesel::expression::IntoSql;
use diesel::prelude::*;
use diesel::sql_types::{Array, Jsonb, Text};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::{Object, Pool};
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use jiff_diesel::ToDiesel;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

// `jsonb_set(target, path, new_value)`. Declaring it here is what keeps
// `WorkspaceStorage::update_reasoning_effort` a single compare-and-set statement
// instead of a read-modify-write: the call is type-checked like any built-in, so
// the argument order and types cannot drift.
diesel::define_sql_function! {
    fn jsonb_set(target: Jsonb, path: Array<Text>, new_value: Jsonb) -> Jsonb;
}

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

/// Serializes migration runs across processes.
///
/// Unlike sqlx's migrator, `diesel_migrations` takes no lock of its own, so two
/// starts racing each other run `create table` concurrently and one loses with a
/// `pg_type_typname_nsp_index` unique violation. Any stable number works; this
/// one is arbitrary and only has to stay put. The lock is session-scoped, so
/// closing the connection releases it even if applying the migrations fails.
const MIGRATION_LOCK: i64 = 0x0C0D_A5E5_5104_1BEF;

/// The process-wide connection pool.
pub type DbPool = Pool<AsyncPgConnection>;

/// Connect to PostgreSQL and bring the schema up to date.
///
/// Migrations are embedded in the binary and run on every start, so deploying is
/// all it takes to create or update the schema. They go over their own one-off
/// connection rather than one borrowed from the pool: applying them blocks the
/// thread (diesel's migration machinery is synchronous, and the harness bridges
/// it with `block_in_place`), which is not something to do while holding a
/// pooled connection. The errors deliberately omit the URL, which carries the
/// password.
pub async fn connect(database_url: &str) -> Result<DbPool, String> {
    let mut conn = AsyncPgConnection::establish(database_url)
        .await
        .map_err(|err| format!("failed to connect to the database: {err}"))?;
    diesel::sql_query("select pg_advisory_lock($1)")
        .bind::<diesel::sql_types::BigInt, _>(MIGRATION_LOCK)
        .execute(&mut conn)
        .await
        .map_err(|err| format!("failed to take the migration lock: {err}"))?;
    let mut harness = diesel_async::AsyncMigrationHarness::new(conn);
    harness
        .run_pending_migrations(MIGRATIONS)
        .map_err(|err| format!("failed to apply database migrations: {err}"))?;
    // Dropping the connection releases the lock.
    drop(harness.into_inner());

    Pool::builder(AsyncDieselConnectionManager::<AsyncPgConnection>::new(
        database_url,
    ))
    .build()
    .map_err(|err| format!("failed to build the database pool: {err}"))
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
#[derive(Clone)]
pub struct WorkspaceStorage {
    pool: DbPool,
    workspace_id: String,
}

// `AsyncPgConnection` is not `Debug`, so neither is the pool wrapping it. What a
// reader wants from these anyway is which rows the handle is scoped to.
impl std::fmt::Debug for WorkspaceStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceStorage")
            .field("workspace_id", &self.workspace_id)
            .finish_non_exhaustive()
    }
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

/// How a session's turn last ended while nobody was attached to see it.
/// Cleared the next time a client attaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnseenOutcome {
    Completed,
    Failed,
}

impl UnseenOutcome {
    /// The canonical string form, shared by the DB column and the wire.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// One row of the session list.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub name: Option<String>,
    pub updated_at_ms: u64,
    pub first_user_message: Option<String>,
    pub has_pending_approval: bool,
    /// `"completed"` / `"failed"`, straight from the DB's `unseen_outcome`
    /// column — not parsed into [`UnseenOutcome`], since the column's `check`
    /// constraint already restricts the value.
    pub unseen_outcome: Option<String>,
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
    pub fn new(pool: DbPool, workspace_id: impl Into<String>) -> Self {
        Self {
            pool,
            workspace_id: workspace_id.into(),
        }
    }

    async fn conn(&self) -> Result<Object<AsyncPgConnection>, String> {
        self.pool
            .get()
            .await
            .map_err(|err| format!("failed to acquire a database connection: {err}"))
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
        let mut conn = self
            .conn()
            .await
            .map_err(SessionMetadataError::Persistence)?;

        diesel::insert_into(sessions::table)
            .values((
                sessions::workspace_id.eq(&self.workspace_id),
                sessions::session_id.eq(session_id),
                sessions::model_binding.eq(Json(requested_binding)),
            ))
            .on_conflict((sessions::workspace_id, sessions::session_id))
            .do_nothing()
            .execute(&mut conn)
            .await
            .map_err(|err| {
                SessionMetadataError::Persistence(format!(
                    "failed to open session {session_id}: {err}"
                ))
            })?;

        sessions::table
            .find((&self.workspace_id, session_id))
            .select(sessions::model_binding)
            .first::<Json<SessionModelBinding>>(&mut conn)
            .await
            .map(Json::into_inner)
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
        let mut conn = self.conn().await.map_err(RenameSessionError::Persistence)?;

        let renamed = diesel::update(sessions::table.find((&self.workspace_id, session_id)))
            .set(sessions::name.eq(name.as_deref()))
            .execute(&mut conn)
            .await
            .map_err(|err| {
                RenameSessionError::Persistence(format!(
                    "failed to rename session {session_id}: {err}"
                ))
            })?;
        if renamed == 0 {
            return Err(RenameSessionError::SessionNotFound);
        }
        Ok(name)
    }

    /// Record that `session_id`'s turn just settled with nobody attached.
    pub async fn mark_unseen_outcome(
        &self,
        session_id: &str,
        outcome: UnseenOutcome,
    ) -> Result<(), String> {
        let mut conn = self.conn().await?;
        diesel::update(sessions::table.find((&self.workspace_id, session_id)))
            .set(sessions::unseen_outcome.eq(outcome.as_str()))
            .execute(&mut conn)
            .await
            .map_err(|err| {
                format!("failed to record the unseen outcome of session {session_id}: {err}")
            })?;
        Ok(())
    }

    /// Clear any unseen outcome recorded for `session_id`. Filtered to skip
    /// the write when there's nothing to clear, since attach is a hot path.
    pub async fn clear_unseen_outcome(&self, session_id: &str) -> Result<(), String> {
        let mut conn = self.conn().await?;
        diesel::update(
            sessions::table
                .find((&self.workspace_id, session_id))
                .filter(sessions::unseen_outcome.is_not_null()),
        )
        .set(sessions::unseen_outcome.eq(None::<&str>))
        .execute(&mut conn)
        .await
        .map_err(|err| {
            format!("failed to clear the unseen outcome of session {session_id}: {err}")
        })?;
        Ok(())
    }

    /// Change a session's reasoning effort, but only while it is still on the
    /// model the caller thinks it is. The `filter` is the compare-and-set: no row
    /// updated means either the session is gone or its binding moved on.
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
        let mut conn = self
            .conn()
            .await
            .map_err(SessionMetadataError::Persistence)?;

        let updated = diesel::update(
            sessions::table
                .find((&self.workspace_id, session_id))
                .filter(
                    sessions::model_binding
                        .retrieve_as_text("provider_id")
                        .eq(expected_provider_id)
                        .and(
                            sessions::model_binding
                                .retrieve_as_text("model_id")
                                .eq(expected_model_id),
                        ),
                ),
        )
        .set(sessions::model_binding.eq(jsonb_set(
            sessions::model_binding,
            vec!["reasoning_effort"],
            Json(effort),
        )))
        .returning(sessions::model_binding)
        .get_result::<Json<SessionModelBinding>>(&mut conn)
        .await
        .optional()
        .map_err(|err| {
            SessionMetadataError::Persistence(format!(
                "failed to update the reasoning effort of session {session_id}: {err}"
            ))
        })?;

        match updated {
            Some(binding) => Ok(binding.into_inner()),
            None if self.session_exists(session_id).await? => {
                Err(SessionMetadataError::BindingMismatch)
            }
            None => Err(SessionMetadataError::SessionNotFound),
        }
    }

    async fn session_exists(&self, session_id: &str) -> Result<bool, SessionMetadataError> {
        let mut conn = self
            .conn()
            .await
            .map_err(SessionMetadataError::Persistence)?;
        diesel::select(diesel::dsl::exists(
            sessions::table.find((&self.workspace_id, session_id)),
        ))
        .get_result(&mut conn)
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
        let mut conn = self.conn().await?;
        diesel::delete(sessions::table.find((&self.workspace_id, session_id)))
            .execute(&mut conn)
            .await
            .map(|_| ())
            .map_err(|err| format!("failed to delete session {session_id}: {err}"))
    }

    /// Copy a session under a freshly minted id, keeping the turns before `cut`.
    /// The source is only read.
    ///
    /// All or nothing, and under one snapshot: the copy runs a statement per
    /// thread, and REPEATABLE READ is what stops those statements from seeing
    /// the source grow between them.
    pub async fn fork_session(
        &self,
        source_id: &str,
        cut: ForkCut,
        source: ForkSource,
    ) -> Result<ForkedSession, ForkError> {
        let new_id = uuid::Uuid::new_v4().to_string();
        let mut conn = self.conn().await.map_err(ForkError::Persistence)?;

        conn.build_transaction()
            .repeatable_read()
            .run(async |conn| {
                let (name, model_binding) = sessions::table
                    .find((&self.workspace_id, source_id))
                    .select((sessions::name, sessions::model_binding))
                    .first::<(Option<String>, Json<SessionModelBinding>)>(conn)
                    .await
                    .optional()?
                    .ok_or(ForkError::SourceNotFound)?;

                // Every thread parked at `Generation` is what makes the source a
                // resting point: nothing is waiting on a tool result, an approval
                // or a sub-agent reply.
                let busy: Option<String> = thread_checkpoints::table
                    .filter(
                        thread_checkpoints::workspace_id
                            .eq(&self.workspace_id)
                            .and(thread_checkpoints::session_id.eq(source_id))
                            .and(
                                thread_checkpoints::resume_point
                                    .ne(Json(StoredResumePoint::Generation)),
                            ),
                    )
                    .select(thread_checkpoints::thread_id)
                    .first(conn)
                    .await
                    .optional()?;
                if let Some(thread_id) = busy {
                    return Err(ForkError::ThreadBusy { thread_id });
                }

                // Checkpoints alone don't prove a cold source is at rest: a task
                // queued behind the last turn outlives shutdown in the runtime
                // snapshot, and `Session::open` replays it, while every
                // checkpoint still reads `Generation`.
                if source == ForkSource::Cold
                    && let Some(Json(snapshot)) = runtime_snapshots::table
                        .find((&self.workspace_id, source_id))
                        .select(runtime_snapshots::snapshot)
                        .first::<Json<StoredRuntimeSnapshot>>(conn)
                        .await
                        .optional()?
                    && let Some(thread_id) = queued_work(&snapshot)
                {
                    return Err(ForkError::SourceNotIdle { thread_id });
                }

                let keep = self.retained_turns(conn, source_id, &cut).await?;

                let checkpoints = thread_checkpoints::table
                    .filter(
                        thread_checkpoints::workspace_id
                            .eq(&self.workspace_id)
                            .and(thread_checkpoints::session_id.eq(source_id)),
                    )
                    .select((
                        thread_checkpoints::thread_id,
                        thread_checkpoints::agent_name,
                        thread_checkpoints::parent_thread_id,
                        thread_checkpoints::derivation_key,
                        thread_checkpoints::reply_target,
                        thread_checkpoints::resume_point,
                        thread_checkpoints::suspended_at,
                    ))
                    .load::<StoredThreadRow>(conn)
                    .await?;
                let thread_ids = remap_thread_ids(&checkpoints, source_id, &new_id)?;

                // How much of each thread survives the cut. Retained turns are
                // always a prefix, so `max_seq + 1 == count` must hold; checking
                // it turns a broken invariant into a rollback instead of a copy
                // whose history has a hole every later save would append past.
                let retained: HashMap<String, (i64, Option<i32>)> = messages::table
                    .filter(
                        messages::workspace_id
                            .eq(&self.workspace_id)
                            .and(messages::session_id.eq(source_id))
                            .and(messages::turn_id.eq_any(&keep)),
                    )
                    .group_by(messages::thread_id)
                    .select((
                        messages::thread_id,
                        diesel::dsl::count_star(),
                        diesel::dsl::max(messages::seq),
                    ))
                    .load::<(String, i64, Option<i32>)>(conn)
                    .await?
                    .into_iter()
                    .map(|(thread_id, count, max_seq)| (thread_id, (count, max_seq)))
                    .collect();

                // Strict insert: `do_nothing` on a collision would pour this
                // session's messages into whatever already holds that id.
                diesel::insert_into(sessions::table)
                    .values((
                        sessions::workspace_id.eq(&self.workspace_id),
                        sessions::session_id.eq(&new_id),
                        sessions::name.eq(&name),
                        sessions::model_binding.eq(model_binding),
                    ))
                    .execute(conn)
                    .await?;

                for checkpoint in &checkpoints {
                    let Some(&(count, max_seq)) = retained.get(&checkpoint.thread_id) else {
                        // Nothing left of this thread: a missing checkpoint and an
                        // empty one restore the same state, and a stateless
                        // thread's id could never be addressed again anyway.
                        continue;
                    };
                    if max_seq.map(|max_seq| i64::from(max_seq) + 1) != Some(count) {
                        return Err(ForkError::HistoryNotContiguous {
                            thread_id: checkpoint.thread_id.clone(),
                        });
                    }
                    let new_thread_id = &thread_ids[&checkpoint.thread_id];

                    // Payloads stay inside the database — some carry inline
                    // base64 images. Tool state is just another column, so the
                    // cut reaches it for free.
                    diesel::insert_into(messages::table)
                        .values(
                            messages::table
                                .filter(
                                    messages::workspace_id
                                        .eq(&self.workspace_id)
                                        .and(messages::session_id.eq(source_id))
                                        .and(messages::thread_id.eq(&checkpoint.thread_id))
                                        .and(messages::turn_id.eq_any(&keep)),
                                )
                                .select((
                                    messages::workspace_id,
                                    new_id.as_str().into_sql::<Text>(),
                                    new_thread_id.as_str().into_sql::<Text>(),
                                    messages::seq,
                                    messages::message_id,
                                    messages::turn_id,
                                    messages::role,
                                    messages::origin_message_id,
                                    messages::origin_call_id,
                                    messages::payload,
                                    messages::state,
                                )),
                        )
                        .into_columns((
                            messages::workspace_id,
                            messages::session_id,
                            messages::thread_id,
                            messages::seq,
                            messages::message_id,
                            messages::turn_id,
                            messages::role,
                            messages::origin_message_id,
                            messages::origin_call_id,
                            messages::payload,
                            messages::state,
                        ))
                        .execute(conn)
                        .await?;

                    let reply_target = checkpoint.reply_target.clone().map(|Json(mut target)| {
                        if let Some(mapped) = thread_ids.get(&target.sender_thread_id) {
                            target.sender_thread_id = mapped.clone();
                        }
                        Json(target)
                    });
                    diesel::insert_into(thread_checkpoints::table)
                        .values((
                            thread_checkpoints::workspace_id.eq(&self.workspace_id),
                            thread_checkpoints::session_id.eq(&new_id),
                            thread_checkpoints::thread_id.eq(new_thread_id),
                            thread_checkpoints::agent_name.eq(&checkpoint.agent_name),
                            thread_checkpoints::parent_thread_id.eq(checkpoint
                                .parent_thread_id
                                .as_ref()
                                .map(|parent| &thread_ids[parent])),
                            thread_checkpoints::derivation_key.eq(&checkpoint.derivation_key),
                            thread_checkpoints::reply_target.eq(reply_target),
                            thread_checkpoints::resume_point.eq(&checkpoint.resume_point),
                            thread_checkpoints::suspended_at.eq(checkpoint.suspended_at),
                            thread_checkpoints::message_count.eq(count as i32),
                            thread_checkpoints::pending_approval
                                .eq(awaits_approval(&checkpoint.resume_point.0)),
                        ))
                        .execute(conn)
                        .await?;
                }

                Ok(ForkedSession {
                    session_id: new_id.clone(),
                    name,
                })
            })
            .await
    }

    /// The turns a fork keeps, as a single predicate over every thread.
    ///
    /// `turn_id` is a UUID and carries no order of its own, so "the turns before
    /// this one" can only be answered by the root thread's `seq`.
    ///
    /// This is rewind's mirror: rewind drops the turns from a user message on
    /// (`seq >= target`), a fork keeps the ones before it.
    async fn retained_turns(
        &self,
        conn: &mut AsyncPgConnection,
        source_id: &str,
        cut: &ForkCut,
    ) -> Result<Vec<uuid::Uuid>, ForkError> {
        let root_thread = messages::table.filter(
            messages::workspace_id
                .eq(&self.workspace_id)
                .and(messages::session_id.eq(source_id))
                .and(messages::thread_id.eq(source_id)),
        );
        let cut_seq = match cut {
            ForkCut::At(target) => Some(
                root_thread
                    .filter(
                        messages::message_id
                            .eq(target.as_uuid())
                            .and(messages::role.eq("user")),
                    )
                    .select(messages::seq)
                    .first::<i32>(conn)
                    .await
                    .optional()?
                    .ok_or(ForkError::CutNotFound)?,
            ),
            // A full copy has no cut to anchor it, and needs none: a turn is
            // announced only once its content is stored, so a source the gate
            // found at rest has nothing left in flight to wait for.
            ForkCut::All => None,
        };

        // No such check for a cut, because the turn it opens is the one turn a
        // fork does *not* keep. A message at `seq = k` is only visible once the
        // transaction that appended it committed, and that transaction wrote
        // `0..k` contiguously — so seeing the cut proves everything kept is
        // stored, however far behind the rest of the session may be.
        let mut turns = root_thread
            .select(messages::turn_id)
            .distinct()
            .into_boxed();
        if let Some(cut_seq) = cut_seq {
            turns = turns.filter(messages::seq.lt(cut_seq));
        }
        Ok(turns.load(conn).await?)
    }

    /// The workspace's sessions, most recently active first.
    ///
    /// The two derived columns stay in the query rather than becoming N+1 reads:
    /// `has_pending_approval` is an `exists` over the session's threads, and
    /// `first_user_message` is a correlated scalar subquery for the opening
    /// message of the root thread (whose `thread_id` is the session id). The
    /// epoch conversion is deliberately *not* pushed into SQL — selecting the
    /// `timestamptz` and converting in Rust keeps the whole query inside the
    /// checked DSL instead of dropping to a raw `extract(...)` fragment.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, String> {
        let mut conn = self.conn().await?;
        let first_user_message = messages::table
            .filter(
                messages::workspace_id
                    .eq(sessions::workspace_id)
                    .and(messages::session_id.eq(sessions::session_id))
                    .and(messages::thread_id.eq(sessions::session_id))
                    .and(messages::role.eq("user")),
            )
            .order(messages::seq)
            .select(messages::payload)
            .limit(1)
            .single_value();
        let has_pending_approval = diesel::dsl::exists(
            thread_checkpoints::table.filter(
                thread_checkpoints::workspace_id
                    .eq(sessions::workspace_id)
                    .and(thread_checkpoints::session_id.eq(sessions::session_id))
                    .and(thread_checkpoints::pending_approval),
            ),
        );

        let rows = sessions::table
            .filter(sessions::workspace_id.eq(&self.workspace_id))
            .select((
                sessions::session_id,
                sessions::name,
                sessions::updated_at,
                has_pending_approval,
                first_user_message,
                sessions::unseen_outcome,
            ))
            .order((sessions::updated_at.desc(), sessions::session_id.asc()))
            .load::<(
                String,
                Option<String>,
                jiff_diesel::Timestamp,
                bool,
                Option<Json<Message>>,
                Option<String>,
            )>(&mut conn)
            .await
            .map_err(|err| format!("failed to list sessions: {err}"))?;

        Ok(rows
            .into_iter()
            .map(
                |(
                    session_id,
                    name,
                    updated_at,
                    has_pending_approval,
                    first_user_message,
                    unseen_outcome,
                )| {
                    SessionSummary {
                        session_id,
                        name,
                        updated_at_ms: updated_at.to_jiff().as_millisecond().max(0) as u64,
                        first_user_message: first_user_message
                            .and_then(|message| session_preview(&message.0)),
                        has_pending_approval,
                        unseen_outcome,
                    }
                },
            )
            .collect())
    }
}

/// Persistence for one session, backed by PostgreSQL.
///
/// Messages are rows, so saving a checkpoint appends only what the thread has
/// gained since the last save instead of rewriting its whole history. The
/// starting point is the thread's own `message_count`: it is both "how many
/// messages are already stored" and "the next free `seq`", which holds because
/// `seq` is exactly the index into the checkpoint's message vector.
#[derive(Clone)]
pub struct PgSessionStorage {
    pool: DbPool,
    workspace_id: String,
    session_id: String,
}

impl std::fmt::Debug for PgSessionStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSessionStorage")
            .field("workspace_id", &self.workspace_id)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

/// The identity and role columns of a message row.
fn message_row_identity(message: &Message) -> (MessageId, &'static str) {
    let role = match message {
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::Tool(_) => "tool",
        Message::Compaction(_) => "compaction",
    };
    (message.message_id(), role)
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

/// A checkpoint save that failed on its own terms rather than on the database's,
/// carried through diesel's transaction plumbing (which insists on an error type
/// it can build from `diesel::result::Error`).
enum SaveError {
    Db(diesel::result::Error),
    Rejected(String),
}

impl From<diesel::result::Error> for SaveError {
    fn from(err: diesel::result::Error) -> Self {
        Self::Db(err)
    }
}

/// Why a compaction was not committed. Either way nothing was written — a
/// caller can retry as it stands, and has no transcript to roll back.
#[derive(Debug)]
pub enum CompactionError {
    /// The root thread grew while the summary was being generated, so the
    /// summary no longer describes the whole thread.
    Stale,
    Persistence(String),
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompactionError::Stale => {
                write!(f, "the conversation changed while it was being summarized")
            }
            CompactionError::Persistence(reason) => write!(f, "{reason}"),
        }
    }
}

impl From<diesel::result::Error> for CompactionError {
    fn from(error: diesel::result::Error) -> Self {
        CompactionError::Persistence(error.to_string())
    }
}

/// Why a rewind changed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewindError {
    /// No such message, or it is not a user message of this session's root
    /// thread. The only identity a client supplies, so this is also what a
    /// forged or stale target lands on.
    TargetNotFound,
    /// A thread is not parked at a plain generation boundary, so the session
    /// still has work in flight and its persisted state is not a resting point.
    ThreadBusy {
        thread_id: String,
    },
    /// A surviving thread's messages are no longer `[0, count)`. Truncation only
    /// ever removes a tail, so this means that invariant broke upstream; the
    /// transaction is rolled back rather than leaving a history with a hole in
    /// it, which every later save would then append past.
    HistoryNotContiguous {
        thread_id: String,
    },
    Persistence(String),
}

impl std::fmt::Display for RewindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetNotFound => write!(f, "no such user message in this session"),
            Self::ThreadBusy { thread_id } => {
                write!(f, "thread {thread_id} is still mid-turn")
            }
            Self::HistoryNotContiguous { thread_id } => write!(
                f,
                "thread {thread_id} would be left with a gap in its history"
            ),
            Self::Persistence(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for RewindError {}

impl From<diesel::result::Error> for RewindError {
    fn from(err: diesel::result::Error) -> Self {
        Self::Persistence(format!("failed to rewind the session: {err}"))
    }
}

/// Where a fork cuts the source session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkCut {
    /// Keep the turns before the one this user message opens. Naming a turn by
    /// the message that opened it is what a rewind does too, and it keeps the
    /// copy clear of the one turn that might still be being written.
    At(MessageId),
    /// Keep everything stored.
    All,
}

/// Whether a runtime is attached to the source while the copy runs.
///
/// The single answer to that question, because two checks need it and storage
/// cannot see it for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkSource {
    /// Attached and at rest.
    Live,
    /// Nothing attached, so the stored state is the whole truth — including a
    /// runtime snapshot, which is only rewritten when an agent exits and
    /// therefore speaks for the source only here.
    Cold,
}

/// A session minted by a fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkedSession {
    pub session_id: String,
    pub name: Option<String>,
}

/// Why a fork produced nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkError {
    /// No such session in this workspace.
    SourceNotFound,
    /// No such user message on the source's root thread.
    CutNotFound,
    /// A thread is not parked at a plain generation boundary.
    ThreadBusy {
        thread_id: String,
    },
    /// A cold source's runtime snapshot still holds work the next open would
    /// resume. The same source would be refused as busy while live.
    SourceNotIdle {
        thread_id: String,
    },
    /// A thread's retained messages are not `[0, count)`. Retained turns are
    /// always a prefix, so this means that invariant broke upstream.
    HistoryNotContiguous {
        thread_id: String,
    },
    /// A checkpoint's parent was never mapped, so the thread graph is not rooted
    /// at the session.
    OrphanThread {
        thread_id: String,
    },
    Persistence(String),
}

impl std::fmt::Display for ForkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceNotFound => write!(f, "no such session"),
            Self::CutNotFound => write!(f, "no such user message in this session"),
            Self::ThreadBusy { thread_id } => write!(f, "thread {thread_id} is still mid-turn"),
            Self::SourceNotIdle { thread_id } => {
                write!(f, "thread {thread_id} has work queued from a previous run")
            }
            Self::HistoryNotContiguous { thread_id } => {
                write!(f, "thread {thread_id} would be copied with a gap")
            }
            Self::OrphanThread { thread_id } => {
                write!(f, "thread {thread_id} has no reachable parent")
            }
            Self::Persistence(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ForkError {}

impl From<diesel::result::Error> for ForkError {
    fn from(err: diesel::result::Error) -> Self {
        Self::Persistence(format!("failed to fork the session: {err}"))
    }
}

/// A source session's checkpoint row, as a fork reads it.
#[derive(Queryable)]
struct StoredThreadRow {
    thread_id: String,
    agent_name: String,
    parent_thread_id: Option<String>,
    derivation_key: Option<String>,
    reply_target: Option<Json<ReplyTarget>>,
    resume_point: Json<StoredResumePoint>,
    suspended_at: jiff_diesel::Timestamp,
}

/// The first thread a persisted snapshot would put back to work on the next
/// open, if any — matching what `Session::open` treats as a resuming session.
fn queued_work(snapshot: &StoredRuntimeSnapshot) -> Option<String> {
    if let Some(thread_id) = snapshot.active_threads.values().next() {
        return Some(thread_id.clone());
    }
    snapshot
        .agent_drained_envelopes
        .values()
        .chain(snapshot.drained_envelopes.values())
        .flatten()
        .next()
        .map(|envelope| envelope.to.thread_id.as_ref().to_string())
}

/// Rebuild every thread id under `new_root`.
///
/// Thread ids are derived, not free: the root thread's id *is* the session id,
/// and each child is `uuid5(parent_thread_id, derivation_key)`. Keeping the old
/// ones would leave the new session unable to find its own root history, and
/// would make a stateful sub-agent miss its thread the next time it is called.
fn remap_thread_ids(
    threads: &[StoredThreadRow],
    old_root: &str,
    new_root: &str,
) -> Result<HashMap<String, String>, ForkError> {
    let mut mapped = HashMap::from([(old_root.to_string(), new_root.to_string())]);
    let mut pending: Vec<&StoredThreadRow> = threads
        .iter()
        .filter(|thread| thread.thread_id != old_root)
        .collect();

    // Parents before children. The graph is shallow, so rescanning beats
    // building an index.
    while !pending.is_empty() {
        let unresolved = pending.len();
        pending.retain(|thread| {
            let (Some(parent), Some(key)) = (&thread.parent_thread_id, &thread.derivation_key)
            else {
                return true;
            };
            let Some(new_parent) = mapped.get(parent).cloned() else {
                return true;
            };
            let derived = ThreadId::from_uuid5(&ThreadId::from(new_parent), key);
            mapped.insert(thread.thread_id.clone(), derived.as_ref().to_string());
            false
        });
        if pending.len() == unresolved {
            return Err(ForkError::OrphanThread {
                thread_id: pending[0].thread_id.clone(),
            });
        }
    }
    Ok(mapped)
}

impl PgSessionStorage {
    pub fn new(
        pool: DbPool,
        workspace_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            workspace_id: workspace_id.into(),
            session_id: session_id.into(),
        }
    }

    async fn conn(&self) -> Result<Object<AsyncPgConnection>, String> {
        self.pool
            .get()
            .await
            .map_err(|err| format!("failed to acquire a database connection: {err}"))
    }

    /// Append the thread's new messages and overwrite its state, atomically.
    async fn write_checkpoint(
        &self,
        thread_id: &str,
        checkpoint: StoredCheckpoint,
    ) -> Result<(), String> {
        let mut conn = self.conn().await?;
        conn.transaction(async |conn| {
            let stored_count: Option<i32> = thread_checkpoints::table
                .find((&self.workspace_id, &self.session_id, thread_id))
                .select(thread_checkpoints::message_count)
                .first(conn)
                .await
                .optional()?;
            let stored_count = stored_count.unwrap_or(0) as usize;

            // History is append-only, which is what makes appending the tail
            // equivalent to rewriting the thread. A shorter checkpoint means
            // someone rewrote history without resetting the count, and appending
            // from the old count would drop messages silently.
            if checkpoint.messages.len() < stored_count {
                return Err(SaveError::Rejected(format!(
                    "thread {thread_id} has {stored_count} stored messages but the checkpoint \
                     carries {}; message history is append-only",
                    checkpoint.messages.len()
                )));
            }

            for (offset, entry) in checkpoint.messages[stored_count..].iter().enumerate() {
                let (message_id, role) = message_row_identity(&entry.message);
                let origin = match &entry.message {
                    Message::User(message) => message.origin.as_ref(),
                    _ => None,
                };
                diesel::insert_into(messages::table)
                    .values((
                        messages::workspace_id.eq(&self.workspace_id),
                        messages::session_id.eq(&self.session_id),
                        messages::thread_id.eq(thread_id),
                        messages::seq.eq((stored_count + offset) as i32),
                        messages::message_id.eq(message_id.as_uuid()),
                        messages::turn_id.eq(entry.turn_id.as_uuid()),
                        messages::role.eq(role),
                        messages::origin_message_id
                            .eq(origin.map(|origin| origin.message_id.as_uuid())),
                        messages::origin_call_id.eq(origin.map(|origin| origin.call_id.as_str())),
                        messages::payload.eq(Json(&entry.message)),
                        // Tool state rides on the row that recorded it, so
                        // `message_count` governs it too and the two cannot
                        // drift apart.
                        messages::state.eq(Json(&entry.state)),
                    ))
                    .execute(conn)
                    .await?;
            }

            let state = (
                thread_checkpoints::agent_name.eq(&checkpoint.agent_name),
                thread_checkpoints::parent_thread_id.eq(&checkpoint.parent_thread_id),
                thread_checkpoints::derivation_key.eq(&checkpoint.derivation_key),
                thread_checkpoints::reply_target.eq(checkpoint.reply_target.as_ref().map(Json)),
                thread_checkpoints::resume_point.eq(Json(&checkpoint.resume_point)),
                thread_checkpoints::suspended_at.eq(checkpoint.suspended_at.to_diesel()),
                thread_checkpoints::message_count.eq(checkpoint.messages.len() as i32),
                thread_checkpoints::pending_approval.eq(awaits_approval(&checkpoint.resume_point)),
            );
            diesel::insert_into(thread_checkpoints::table)
                .values((
                    thread_checkpoints::workspace_id.eq(&self.workspace_id),
                    thread_checkpoints::session_id.eq(&self.session_id),
                    thread_checkpoints::thread_id.eq(thread_id),
                    state.clone(),
                ))
                .on_conflict((
                    thread_checkpoints::workspace_id,
                    thread_checkpoints::session_id,
                    thread_checkpoints::thread_id,
                ))
                .do_update()
                .set(state)
                .execute(conn)
                .await?;

            touch(conn, &self.workspace_id, &self.session_id).await?;
            Ok(())
        })
        .await
        .map_err(|err| match err {
            SaveError::Rejected(message) => message,
            SaveError::Db(err) => format!("failed to save the checkpoint of {thread_id}: {err}"),
        })
    }

    async fn read_checkpoint(&self, thread_id: &str) -> Result<Option<StoredCheckpoint>, String> {
        let mut conn = self.conn().await?;
        let state = thread_checkpoints::table
            .find((&self.workspace_id, &self.session_id, thread_id))
            .select((
                thread_checkpoints::agent_name,
                thread_checkpoints::parent_thread_id,
                thread_checkpoints::derivation_key,
                thread_checkpoints::reply_target,
                thread_checkpoints::resume_point,
                thread_checkpoints::suspended_at,
            ))
            .first::<(
                String,
                Option<String>,
                Option<String>,
                Option<Json<ReplyTarget>>,
                Json<StoredResumePoint>,
                jiff_diesel::Timestamp,
            )>(&mut conn)
            .await
            .optional()
            .map_err(|err| format!("failed to load the state of {thread_id}: {err}"))?;
        let Some((
            agent_name,
            parent_thread_id,
            derivation_key,
            reply_target,
            resume_point,
            suspended_at,
        )) = state
        else {
            return Ok(None);
        };

        let messages = messages::table
            .filter(
                messages::workspace_id
                    .eq(&self.workspace_id)
                    .and(messages::session_id.eq(&self.session_id))
                    .and(messages::thread_id.eq(thread_id)),
            )
            .order(messages::seq)
            .select((messages::turn_id, messages::payload, messages::state))
            .load::<(uuid::Uuid, Json<Message>, Json<ThreadStateMap>)>(&mut conn)
            .await
            .map_err(|err| format!("failed to load the messages of {thread_id}: {err}"))?
            .into_iter()
            .map(|(turn_id, payload, state)| HistoryEntry {
                turn_id: TurnId::from(MessageId::from(turn_id)),
                message: payload.0,
                state: state.into_inner(),
            })
            .collect();

        Ok(Some(StoredCheckpoint {
            thread_id: thread_id.to_string(),
            agent_name,
            parent_thread_id,
            derivation_key,
            reply_target: reply_target.map(Json::into_inner),
            messages,
            resume_point: resume_point.into_inner(),
            suspended_at: suspended_at.to_jiff(),
        }))
    }

    async fn read_pending_approval_checkpoints(&self) -> Result<Vec<StoredCheckpoint>, String> {
        let thread_ids = {
            let mut conn = self.conn().await?;
            thread_checkpoints::table
                .filter(
                    thread_checkpoints::workspace_id
                        .eq(&self.workspace_id)
                        .and(thread_checkpoints::session_id.eq(&self.session_id))
                        .and(thread_checkpoints::pending_approval),
                )
                .order(thread_checkpoints::thread_id)
                .select(thread_checkpoints::thread_id)
                .load::<String>(&mut conn)
                .await
                .map_err(|err| format!("failed to list pending approvals: {err}"))?
        };

        let mut checkpoints = Vec::with_capacity(thread_ids.len());
        for thread_id in thread_ids {
            let checkpoint = self
                .read_checkpoint(&thread_id)
                .await?
                .ok_or_else(|| format!("pending approval checkpoint {thread_id} disappeared"))?;
            checkpoints.push(checkpoint);
        }
        Ok(checkpoints)
    }

    /// Append a compaction's two messages to the root thread, provided nothing
    /// has appended to it since `expected_message_count` was read.
    ///
    /// The caller reads that count when it takes the history to summarize, so
    /// the comparison means exactly "what I summarized is still the whole
    /// thread". Between the two the caller holds no lock — the summary is a
    /// full LLM round-trip — and the session may be released, reopened, and
    /// given a new turn in the meantime. Appending then would put the boundary
    /// *after* messages the summary never saw, cutting them out of the model's
    /// view; so a mismatch rolls back and writes nothing at all.
    pub async fn commit_compaction(
        &self,
        expected_message_count: usize,
        turn_id: TurnId,
        messages: [&Message; 2],
    ) -> Result<(), CompactionError> {
        let mut conn = self.conn().await.map_err(CompactionError::Persistence)?;
        conn.transaction(async |conn| {
            // The root thread's id is the session id. Updating the watermark
            // under the expected value *is* the compare-and-swap: the row lock
            // it takes also serializes this against a concurrent save.
            let claimed = diesel::update(
                thread_checkpoints::table.filter(
                    thread_checkpoints::workspace_id
                        .eq(&self.workspace_id)
                        .and(thread_checkpoints::session_id.eq(&self.session_id))
                        .and(thread_checkpoints::thread_id.eq(&self.session_id))
                        .and(thread_checkpoints::message_count.eq(expected_message_count as i32)),
                ),
            )
            .set(
                thread_checkpoints::message_count
                    .eq((expected_message_count + messages.len()) as i32),
            )
            .execute(conn)
            .await?;
            if claimed == 0 {
                return Err(CompactionError::Stale);
            }

            for (offset, message) in messages.into_iter().enumerate() {
                let (message_id, role) = message_row_identity(message);
                diesel::insert_into(messages::table)
                    .values((
                        messages::workspace_id.eq(&self.workspace_id),
                        messages::session_id.eq(&self.session_id),
                        messages::thread_id.eq(&self.session_id),
                        messages::seq.eq((expected_message_count + offset) as i32),
                        messages::message_id.eq(message_id.as_uuid()),
                        messages::turn_id.eq(turn_id.as_uuid()),
                        messages::role.eq(role),
                        // Origin names the sub-agent call that opened a thread;
                        // these two are appended to the root thread, which
                        // nothing calls.
                        messages::origin_message_id.eq(None::<uuid::Uuid>),
                        messages::origin_call_id.eq(None::<&str>),
                        messages::payload.eq(Json(message)),
                    ))
                    .execute(conn)
                    .await?;
            }

            touch(conn, &self.workspace_id, &self.session_id).await?;
            Ok(())
        })
        .await
    }

    /// Discard `target` and everything the session produced from it onward, then
    /// report the root thread's remaining conversation.
    ///
    /// Rejects — changing nothing — when `target` is not a user message of this
    /// session's root thread, or when any of the session's threads is not parked
    /// at a plain generation boundary.
    ///
    /// The caller must have stopped the session's runtime first. This cannot
    /// check that, and it matters: an agent that is still flushing a checkpoint
    /// would write back the very tail this removes, and against a lowered
    /// `message_count` that write looks like ordinary growth.
    pub async fn rewind_to(&self, target: MessageId) -> Result<Vec<Message>, RewindError> {
        let mut conn = self.conn().await.map_err(RewindError::Persistence)?;
        conn.transaction(async |conn| {
            // The root thread's id is the session id, so "a user message of this
            // session's root thread" is the whole check the one client-supplied
            // identity has to pass.
            let target_seq: i32 = messages::table
                .filter(
                    messages::workspace_id
                        .eq(&self.workspace_id)
                        .and(messages::session_id.eq(&self.session_id))
                        .and(messages::thread_id.eq(&self.session_id))
                        .and(messages::message_id.eq(target.as_uuid()))
                        .and(messages::role.eq("user")),
                )
                .select(messages::seq)
                .first(conn)
                .await
                .optional()?
                .ok_or(RewindError::TargetNotFound)?;

            // Every thread sitting at `Generation` is what makes the rest of this
            // safe: nothing is waiting on a tool result, an approval or a
            // sub-agent reply, so no queued envelope has anyone expecting it and
            // the runtime snapshot below can go.
            let busy: Option<String> = thread_checkpoints::table
                .filter(
                    thread_checkpoints::workspace_id
                        .eq(&self.workspace_id)
                        .and(thread_checkpoints::session_id.eq(&self.session_id))
                        .and(
                            thread_checkpoints::resume_point
                                .ne(Json(StoredResumePoint::Generation)),
                        ),
                )
                .select(thread_checkpoints::thread_id)
                .first(conn)
                .await
                .optional()?;
            if let Some(thread_id) = busy {
                return Err(RewindError::ThreadBusy { thread_id });
            }

            // A turn is named by the root user message that opened it, and one
            // submission tags every message it causes in every thread. So the
            // turns to drop are the ones the root thread carries from the target
            // on, and dropping them is a single predicate over the whole session
            // — no walking the call graph.
            let discarded: Vec<uuid::Uuid> = messages::table
                .filter(
                    messages::workspace_id
                        .eq(&self.workspace_id)
                        .and(messages::session_id.eq(&self.session_id))
                        .and(messages::thread_id.eq(&self.session_id))
                        .and(messages::seq.ge(target_seq)),
                )
                .select(messages::turn_id)
                .distinct()
                .load(conn)
                .await?;
            diesel::delete(
                messages::table.filter(
                    messages::workspace_id
                        .eq(&self.workspace_id)
                        .and(messages::session_id.eq(&self.session_id))
                        .and(messages::turn_id.eq_any(discarded)),
                ),
            )
            .execute(conn)
            .await?;

            // `message_count` is the watermark every later append starts from, so
            // it has to come down with the messages it counted.
            let remaining: HashMap<String, (i64, Option<i32>)> = messages::table
                .filter(
                    messages::workspace_id
                        .eq(&self.workspace_id)
                        .and(messages::session_id.eq(&self.session_id)),
                )
                .group_by(messages::thread_id)
                .select((
                    messages::thread_id,
                    diesel::dsl::count_star(),
                    diesel::dsl::max(messages::seq),
                ))
                .load::<(String, i64, Option<i32>)>(conn)
                .await?
                .into_iter()
                .map(|(thread_id, count, max_seq)| (thread_id, (count, max_seq)))
                .collect();

            let threads: Vec<String> = thread_checkpoints::table
                .filter(
                    thread_checkpoints::workspace_id
                        .eq(&self.workspace_id)
                        .and(thread_checkpoints::session_id.eq(&self.session_id)),
                )
                .select(thread_checkpoints::thread_id)
                .load(conn)
                .await?;
            for thread_id in threads {
                let Some(&(count, max_seq)) = remaining.get(&thread_id) else {
                    // A thread with nothing left has nothing to resume from and
                    // nothing to say. For a stateless thread this is the only
                    // correct outcome — its id was derived from the assistant
                    // message that called it, which is now gone, so nothing could
                    // ever address the row again — and for a stateful one it is
                    // indistinguishable from keeping an empty row, since a
                    // missing checkpoint and an empty one restore the same state.
                    diesel::delete(thread_checkpoints::table.find((
                        &self.workspace_id,
                        &self.session_id,
                        &thread_id,
                    )))
                    .execute(conn)
                    .await?;
                    continue;
                };
                // Turns accumulate in order within a thread, so what was removed
                // is always a tail and the rest still runs `0..count`. Checking it
                // here turns a broken invariant into a rolled-back transaction
                // instead of a history with a hole that every later save appends
                // past.
                if max_seq.map(|max_seq| i64::from(max_seq) + 1) != Some(count) {
                    return Err(RewindError::HistoryNotContiguous { thread_id });
                }
                diesel::update(thread_checkpoints::table.find((
                    &self.workspace_id,
                    &self.session_id,
                    &thread_id,
                )))
                .set(thread_checkpoints::message_count.eq(count as i32))
                .execute(conn)
                .await?;
            }

            // Whatever the runtime had queued belongs to a turn that just went
            // away, and the `Generation` check above proved nobody is waiting on
            // it. Keeping the row would replay it into the rebuilt runtime.
            diesel::delete(runtime_snapshots::table.find((&self.workspace_id, &self.session_id)))
                .execute(conn)
                .await?;

            touch(conn, &self.workspace_id, &self.session_id).await?;

            Ok(messages::table
                .filter(
                    messages::workspace_id
                        .eq(&self.workspace_id)
                        .and(messages::session_id.eq(&self.session_id))
                        .and(messages::thread_id.eq(&self.session_id)),
                )
                .order(messages::seq)
                .select(messages::payload)
                .load::<Json<Message>>(conn)
                .await?
                .into_iter()
                .map(Json::into_inner)
                .collect())
        })
        .await
    }
}

/// Record that the session changed, so the session list orders by it. Free
/// function rather than a method so it can run inside a transaction closure,
/// which already holds the connection.
async fn touch(
    conn: &mut AsyncPgConnection,
    workspace_id: &str,
    session_id: &str,
) -> Result<(), diesel::result::Error> {
    diesel::update(sessions::table.find((workspace_id, session_id)))
        .set(sessions::updated_at.eq(diesel::dsl::now))
        .execute(conn)
        .await
        .map(|_| ())
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

    fn load_pending_approval_checkpoints(
        &self,
        _session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<StoredCheckpoint>, String>> + Send + '_>> {
        Box::pin(async move { self.read_pending_approval_checkpoints().await })
    }

    fn save_session_snapshot(
        &self,
        _session_id: String,
        snapshot: StoredRuntimeSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            conn.transaction(async |conn| {
                diesel::insert_into(runtime_snapshots::table)
                    .values((
                        runtime_snapshots::workspace_id.eq(&self.workspace_id),
                        runtime_snapshots::session_id.eq(&self.session_id),
                        runtime_snapshots::snapshot.eq(Json(&snapshot)),
                    ))
                    .on_conflict((
                        runtime_snapshots::workspace_id,
                        runtime_snapshots::session_id,
                    ))
                    .do_update()
                    .set(runtime_snapshots::snapshot.eq(Json(&snapshot)))
                    .execute(conn)
                    .await?;
                touch(conn, &self.workspace_id, &self.session_id).await
            })
            .await
            .map_err(|err| format!("failed to save the runtime snapshot: {err}"))
        })
    }

    fn load_session_snapshot(
        &self,
        _session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<StoredRuntimeSnapshot>, String>> + Send + '_>>
    {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            runtime_snapshots::table
                .find((&self.workspace_id, &self.session_id))
                .select(runtime_snapshots::snapshot)
                .first::<Json<StoredRuntimeSnapshot>>(&mut conn)
                .await
                .optional()
                .map(|snapshot| snapshot.map(Json::into_inner))
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
