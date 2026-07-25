//! PostgreSQL storage tests.
//!
//! `DATABASE_URL` must point at a *throwaway* database: this suite runs the
//! migrations against it and writes and deletes rows. Nothing here skips when
//! the database is missing — PostgreSQL is the only production storage backend,
//! and a suite that skips itself would leave persistence completely untested
//! while CI still reported green.
//!
//! Each test scopes itself to a random `workspace_id`. Every table is keyed on
//! `(workspace_id, session_id)` and `WorkspaceStorage` is workspace-scoped, so
//! tests never see each other's rows and can run in parallel without cleanup.

use coda_agent::HistoryEntry;
use coda_agent::agent::ReplyTarget;
use coda_agent::persist::{StoredCheckpoint, StoredResumePoint, StoredRuntimeSnapshot};
use coda_agent::runtime::SessionStorage;
use coda_core::llm::{
    AssistantMessage, Message, MessageId, MessageOrigin, ReasoningContinuation, ToolCall,
    ToolCallOutcome, ToolMessage, ToolOutput, TurnId, UserMessage,
};
use coda_server::storage::DbPool;
use coda_server::storage::{
    PgSessionStorage, RenameSessionError, SessionMetadataError, SessionModelBinding,
    WorkspaceStorage,
};
use coda_tools::TodoItem;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Bool, Integer, Nullable, Text};
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::deadpool::Object;

/// A fresh pool per test. A pool is tied to the runtime that created it and
/// `#[tokio::test]` gives every test its own, so a pool shared through a static
/// starts timing out the moment the first test's runtime shuts down. Connections
/// are opened on demand, and the migrator takes a PostgreSQL advisory lock, so
/// paying for a pool per test costs one connection and one version check.
///
/// Every test here runs on a multi-threaded runtime because applying the
/// migrations goes through `block_in_place`, which panics on the current-thread
/// flavour `#[tokio::test]` would otherwise give it.
async fn pool() -> DbPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must point at a throwaway PostgreSQL database \
         (this suite migrates, writes and deletes)",
    );
    coda_server::storage::connect(&url)
        .await
        .expect("connect to DATABASE_URL and apply migrations")
}

/// A connection for the raw-SQL probes below. Those deliberately go around the
/// storage API to check what actually landed in the database — column values,
/// row versions, cascade behaviour — which is not something the DSL should be
/// asked to express.
async fn conn(pool: &DbPool) -> Object<AsyncPgConnection> {
    pool.get().await.expect("a pooled connection")
}

#[derive(QueryableByName)]
struct SessionIdRow {
    #[diesel(sql_type = Text)]
    session_id: String,
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

#[derive(QueryableByName)]
struct BoolRow {
    #[diesel(sql_type = Bool)]
    ok: bool,
}

/// `xmin` is the transaction that last wrote the row, cast to text so it needs no
/// special type support.
#[derive(QueryableByName)]
struct RowVersion {
    #[diesel(sql_type = Integer)]
    seq: i32,
    #[diesel(sql_type = Text)]
    version: String,
}

#[derive(QueryableByName)]
struct ThreadSeqRow {
    #[diesel(sql_type = Text)]
    thread_id: String,
    #[diesel(sql_type = Integer)]
    seq: i32,
}

/// The columns split out of a message's payload, joined to its thread's state.
#[derive(QueryableByName)]
struct SplitColumnsRow {
    #[diesel(sql_type = Text)]
    role: String,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    turn_id: uuid::Uuid,
    #[diesel(sql_type = Nullable<diesel::sql_types::Uuid>)]
    origin_message_id: Option<uuid::Uuid>,
    #[diesel(sql_type = Nullable<Text>)]
    origin_call_id: Option<String>,
    #[diesel(sql_type = Bool)]
    pending_approval: bool,
    #[diesel(sql_type = Integer)]
    message_count: i32,
}

fn workspace_id(test: &str) -> String {
    format!("{test}-{}", uuid::Uuid::new_v4())
}

fn test_binding() -> SessionModelBinding {
    SessionModelBinding {
        provider_id: "openrouter".to_string(),
        model_id: "x-ai/grok-4.5".to_string(),
        reasoning_effort: Some("high".to_string()),
    }
}

/// The session row everything else hangs off, written the way production does.
async fn seed_session(pool: &DbPool, workspace: &str, session: &str) {
    WorkspaceStorage::new(pool.clone(), workspace)
        .initialize_session(session, test_binding())
        .await
        .unwrap();
}

/// A thread state with nothing interesting in it, so a test can show only the
/// fields it is about.
fn checkpoint(thread_id: &str, messages: Vec<HistoryEntry>) -> StoredCheckpoint {
    StoredCheckpoint {
        thread_id: thread_id.to_string(),
        agent_name: "coda".to_string(),
        parent_thread_id: None,
        derivation_key: None,
        reply_target: None,
        messages,
        todos: vec![],
        resume_point: StoredResumePoint::Generation,
        suspended_at: jiff::Timestamp::default(),
    }
}

fn entry(turn_id: TurnId, message: Message) -> HistoryEntry {
    HistoryEntry { turn_id, message }
}

/// A plain assistant reply, so tests that only need "something the agent said"
/// don't spell out ten fields of timing and reasoning state.
fn assistant(content: &str) -> Message {
    Message::Assistant(AssistantMessage {
        message_id: MessageId::new(),
        content: content.to_string(),
        tool_calls: vec![],
        usage: None,
        reasoning_content: None,
        reasoning_continuation: None,
        reasoning_ended_at: None,
        aborted: false,
        started_at: jiff::Timestamp::default(),
        ended_at: jiff::Timestamp::default(),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_takes_its_threads_messages_and_snapshot_with_it() {
    let pool = pool().await;
    let workspace = workspace_id("cascade");

    let mut conn = conn(&pool).await;

    diesel::sql_query(
        "insert into sessions (workspace_id, session_id, model_binding)
         values ($1, 'doomed', '{}'::jsonb), ($1, 'keeper', '{}'::jsonb)",
    )
    .bind::<Text, _>(&workspace)
    .execute(&mut conn)
    .await
    .unwrap();

    for session in ["doomed", "keeper"] {
        diesel::sql_query(
            "insert into thread_checkpoints
                (workspace_id, session_id, thread_id, agent_name, resume_point,
                 todos, suspended_at, message_count, pending_approval)
             values ($1, $2, $2, 'coda', '\"Generation\"'::jsonb, '[]'::jsonb, now(), 1, false)",
        )
        .bind::<Text, _>(&workspace)
        .bind::<Text, _>(session)
        .execute(&mut conn)
        .await
        .unwrap();
        diesel::sql_query(
            "insert into messages
                (workspace_id, session_id, thread_id, seq, message_id, turn_id, role, payload)
             values ($1, $2, $2, 0, gen_random_uuid(), gen_random_uuid(), 'user', '{}'::jsonb)",
        )
        .bind::<Text, _>(&workspace)
        .bind::<Text, _>(session)
        .execute(&mut conn)
        .await
        .unwrap();
        diesel::sql_query(
            "insert into runtime_snapshots (workspace_id, session_id, snapshot)
             values ($1, $2, '{}'::jsonb)",
        )
        .bind::<Text, _>(&workspace)
        .bind::<Text, _>(session)
        .execute(&mut conn)
        .await
        .unwrap();
    }

    diesel::sql_query("delete from sessions where workspace_id = $1 and session_id = 'doomed'")
        .bind::<Text, _>(&workspace)
        .execute(&mut conn)
        .await
        .unwrap();

    // Everything owned by the deleted session is gone, and the sibling session
    // is untouched — the cascade follows the composite key, not just the id.
    for table in ["thread_checkpoints", "messages", "runtime_snapshots"] {
        let surviving: Vec<String> = diesel::sql_query(format!(
            "select session_id from {table} where workspace_id = $1"
        ))
        .bind::<Text, _>(&workspace)
        .load::<SessionIdRow>(&mut conn)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.session_id)
        .collect();
        assert_eq!(surviving, vec!["keeper"], "{table} was not cascaded");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thread_cannot_belong_to_a_session_that_does_not_exist() {
    let pool = pool().await;
    let workspace = workspace_id("orphan");

    let orphan = diesel::sql_query(
        "insert into thread_checkpoints
            (workspace_id, session_id, thread_id, agent_name, resume_point,
             todos, suspended_at, message_count, pending_approval)
         values ($1, 'never-opened', 'never-opened', 'coda', '\"Generation\"'::jsonb,
                 '[]'::jsonb, now(), 0, false)",
    )
    .bind::<Text, _>(&workspace)
    .execute(&mut conn(&pool).await)
    .await;

    assert!(
        orphan.is_err(),
        "the foreign key must reject a checkpoint with no session row"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_thread_comes_back_whole() {
    let pool = pool().await;
    let workspace = workspace_id("round-trip");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let turn = TurnId::from(MessageId::new());
    let opening_call = MessageOrigin {
        message_id: MessageId::new(),
        call_id: "call_explore".to_string(),
    };
    // Sub-microsecond digits: PostgreSQL stores microseconds, so the write
    // truncates there. Every consumer reads this at millisecond granularity.
    let suspended_at = jiff::Timestamp::now();
    let saved = StoredCheckpoint {
        agent_name: "explore".to_string(),
        parent_thread_id: Some("chat".to_string()),
        derivation_key: Some(opening_call.derivation_key()),
        reply_target: Some(ReplyTarget {
            envelope_id: "env-1".to_string(),
            sender_name: "coda".to_string(),
            sender_thread_id: "chat".to_string(),
            call_id: "call_explore".to_string(),
        }),
        todos: vec![TodoItem {
            title: "read the schema".to_string(),
            done: true,
        }],
        resume_point: StoredResumePoint::PendingApproval {
            parent_message_id: MessageId::new(),
            pending_approval_calls: vec![ToolCall {
                id: "call_shell".to_string(),
                name: "shell".to_string(),
                arguments: Some(r#"{"command":"ls"}"#.to_string()),
            }],
            pending_calls: vec![],
        },
        suspended_at,
        ..checkpoint(
            "explore-thread",
            vec![
                entry(
                    turn,
                    Message::User(UserMessage::from_subagent_call(
                        MessageId::new(),
                        "look into the schema",
                        opening_call.clone(),
                    )),
                ),
                entry(turn, assistant("on it")),
                entry(
                    turn,
                    Message::Tool(ToolMessage::new(
                        "call_shell",
                        "shell",
                        ToolOutput::Ok("migrations/".to_string()),
                        ToolCallOutcome::Approved,
                        None,
                    )),
                ),
            ],
        )
    };

    storage
        .save_checkpoint("explore-thread".to_string(), saved.clone())
        .await
        .unwrap();
    let loaded = storage
        .load_checkpoint("explore-thread")
        .await
        .unwrap()
        .expect("the checkpoint was just saved");

    assert_eq!(loaded.thread_id, "explore-thread");
    assert_eq!(loaded.agent_name, "explore");
    assert_eq!(loaded.parent_thread_id.as_deref(), Some("chat"));
    assert_eq!(
        loaded.derivation_key,
        Some(opening_call.derivation_key()),
        "the derivation key is what lets a fork rebuild this thread's id"
    );
    assert_eq!(
        loaded.reply_target.map(|target| target.envelope_id),
        Some("env-1".to_string())
    );
    assert_eq!(loaded.todos.len(), 1);
    assert!(matches!(
        loaded.resume_point,
        StoredResumePoint::PendingApproval { .. }
    ));
    assert_eq!(
        loaded.suspended_at,
        jiff::Timestamp::from_microsecond(suspended_at.as_microsecond()).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&loaded.messages).unwrap(),
        serde_json::to_value(&saved.messages).unwrap(),
        "the conversation must survive the row split byte for byte"
    );

    // The columns split out of the payload carry the same values the payload does,
    // which is what makes them safe to query on.
    let row = diesel::sql_query(
        "select role, turn_id, origin_message_id, origin_call_id, pending_approval, message_count
           from messages
           join thread_checkpoints using (workspace_id, session_id, thread_id)
          where workspace_id = $1 and seq = 0",
    )
    .bind::<Text, _>(&workspace)
    .get_result::<SplitColumnsRow>(&mut conn(&pool).await)
    .await
    .unwrap();
    assert_eq!(row.role, "user");
    assert_eq!(row.turn_id, turn.as_uuid());
    assert_eq!(
        row.origin_message_id,
        Some(opening_call.message_id.as_uuid())
    );
    assert_eq!(row.origin_call_id.as_deref(), Some("call_explore"));
    assert!(row.pending_approval);
    assert_eq!(row.message_count, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn saving_twice_appends_only_the_new_messages() {
    let pool = pool().await;
    let workspace = workspace_id("append");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let first_turn = TurnId::from(MessageId::new());
    let mut messages = vec![
        entry(
            first_turn,
            Message::User(UserMessage::text(MessageId::new(), "hello")),
        ),
        entry(first_turn, assistant("hi")),
    ];
    storage
        .save_checkpoint("chat".to_string(), checkpoint("chat", messages.clone()))
        .await
        .unwrap();

    // `xmin` is the transaction that last wrote the row, so it changes if a row
    // is rewritten rather than left alone.
    let versions = diesel::sql_query(
        "select seq, xmin::text as version from messages where workspace_id = $1 order by seq",
    )
    .bind::<Text, _>(&workspace)
    .load::<RowVersion>(&mut conn(&pool).await)
    .await
    .unwrap();
    let first_versions: Vec<String> = versions.iter().map(|row| row.version.clone()).collect();
    assert_eq!(first_versions.len(), 2);

    let second_turn = TurnId::from(MessageId::new());
    messages.push(entry(
        second_turn,
        Message::User(UserMessage::text(MessageId::new(), "and now?")),
    ));
    messages.push(entry(second_turn, assistant("now this")));
    storage
        .save_checkpoint("chat".to_string(), checkpoint("chat", messages.clone()))
        .await
        .unwrap();

    let versions = diesel::sql_query(
        "select seq, xmin::text as version from messages where workspace_id = $1 order by seq",
    )
    .bind::<Text, _>(&workspace)
    .load::<RowVersion>(&mut conn(&pool).await)
    .await
    .unwrap();
    assert_eq!(versions.len(), 4, "the second save must add two rows");
    assert_eq!(
        versions[..2]
            .iter()
            .map(|row| row.version.clone())
            .collect::<Vec<_>>(),
        first_versions,
        "the messages saved the first time must not be rewritten"
    );
    assert_eq!(
        versions.iter().map(|row| row.seq).collect::<Vec<_>>(),
        vec![0, 1, 2, 3],
        "seq stays the contiguous index into the message vector"
    );

    let loaded = storage.load_checkpoint("chat").await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&loaded.messages).unwrap(),
        serde_json::to_value(&messages).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkpoint_that_lost_messages_is_refused() {
    let pool = pool().await;
    let workspace = workspace_id("shrunk");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let turn = TurnId::from(MessageId::new());
    let messages = vec![
        entry(
            turn,
            Message::User(UserMessage::text(MessageId::new(), "hello")),
        ),
        entry(turn, assistant("hi")),
    ];
    storage
        .save_checkpoint("chat".to_string(), checkpoint("chat", messages.clone()))
        .await
        .unwrap();

    // Appending "everything past the stored count" is only equivalent to
    // rewriting the thread while history is append-only. A shorter history means
    // that stopped being true, and continuing would drop messages silently.
    let shrunk = storage
        .save_checkpoint(
            "chat".to_string(),
            checkpoint("chat", messages[..1].to_vec()),
        )
        .await;

    assert!(
        shrunk
            .as_ref()
            .is_err_and(|err| err.contains("append-only")),
        "expected an append-only complaint, got {shrunk:?}"
    );
    let stored = diesel::sql_query("select count(*) from messages where workspace_id = $1")
        .bind::<Text, _>(&workspace)
        .get_result::<CountRow>(&mut conn(&pool).await)
        .await
        .unwrap()
        .count;
    assert_eq!(stored, 2, "the refused save must not have changed anything");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_assistant_message_keeps_its_reasoning_continuation() {
    let pool = pool().await;
    let workspace = workspace_id("continuation");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let now = jiff::Timestamp::now();
    let turn = TurnId::from(MessageId::new());
    storage
        .save_checkpoint(
            "chat".to_string(),
            checkpoint(
                "chat",
                vec![entry(
                    turn,
                    Message::Assistant(AssistantMessage {
                        message_id: MessageId::new(),
                        content: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "call_weather".to_string(),
                            name: "lookup_weather".to_string(),
                            arguments: Some(r#"{"city":"Singapore"}"#.to_string()),
                        }],
                        usage: None,
                        reasoning_content: Some("Need current weather.".to_string()),
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
                    }),
                )],
            ),
        )
        .await
        .unwrap();

    let loaded = storage.load_checkpoint("chat").await.unwrap().unwrap();
    let Message::Assistant(message) = &loaded.messages[0].message else {
        panic!("expected an assistant message");
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
        ])),
        "the opaque provider payload must survive verbatim"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn one_submission_is_recoverable_across_every_thread_it_reached() {
    let pool = pool().await;
    let workspace = workspace_id("turn");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let first = TurnId::from(MessageId::new());
    let second = TurnId::from(MessageId::new());
    storage
        .save_checkpoint(
            "chat".to_string(),
            checkpoint(
                "chat",
                vec![
                    entry(
                        first,
                        Message::User(UserMessage::text(MessageId::new(), "explore")),
                    ),
                    entry(first, assistant("delegating")),
                    entry(
                        second,
                        Message::User(UserMessage::text(MessageId::new(), "anything else?")),
                    ),
                ],
            ),
        )
        .await
        .unwrap();
    // The sub-agent's own thread, carrying the same turn as the submission that
    // reached it.
    storage
        .save_checkpoint(
            "explore-thread".to_string(),
            checkpoint(
                "explore-thread",
                vec![
                    entry(
                        first,
                        Message::User(UserMessage::text(MessageId::new(), "look around")),
                    ),
                    entry(first, assistant("found it")),
                ],
            ),
        )
        .await
        .unwrap();

    // One predicate collects a submission's whole fan-out — no walking `origin`
    // up the thread tree. This is what a rewind will truncate on.
    let reached: Vec<(String, i32)> = diesel::sql_query(
        "select thread_id, seq from messages
          where workspace_id = $1 and session_id = 'chat' and turn_id = $2
          order by thread_id, seq",
    )
    .bind::<Text, _>(&workspace)
    .bind::<diesel::sql_types::Uuid, _>(first.as_uuid())
    .load::<ThreadSeqRow>(&mut conn(&pool).await)
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.thread_id, row.seq))
    .collect();

    assert_eq!(
        reached,
        vec![
            ("chat".to_string(), 0),
            ("chat".to_string(), 1),
            ("explore-thread".to_string(), 0),
            ("explore-thread".to_string(), 1),
        ],
        "the turn must find both threads, and only the first submission's messages"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stateful_sub_agent_thread_grows_across_calls() {
    let pool = pool().await;
    let workspace = workspace_id("stateful");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    // A stateful sub-agent keeps one thread id across invocations, so its second
    // call appends to the history the first one left behind.
    let first_call = TurnId::from(MessageId::new());
    let mut history = vec![
        entry(
            first_call,
            Message::User(UserMessage::text(MessageId::new(), "first question")),
        ),
        entry(first_call, assistant("first answer")),
    ];
    storage
        .save_checkpoint(
            "explore-thread".to_string(),
            checkpoint("explore-thread", history.clone()),
        )
        .await
        .unwrap();

    let second_call = TurnId::from(MessageId::new());
    history.push(entry(
        second_call,
        Message::User(UserMessage::text(MessageId::new(), "second question")),
    ));
    history.push(entry(second_call, assistant("second answer")));
    storage
        .save_checkpoint(
            "explore-thread".to_string(),
            checkpoint("explore-thread", history.clone()),
        )
        .await
        .unwrap();

    let loaded = storage
        .load_checkpoint("explore-thread")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.messages.len(), 4);
    assert_eq!(
        loaded
            .messages
            .iter()
            .map(|entry| entry.turn_id.to_string())
            .collect::<Vec<_>>(),
        vec![
            first_call.to_string(),
            first_call.to_string(),
            second_call.to_string(),
            second_call.to_string(),
        ],
        "each call's messages stay tagged with the submission that caused them"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_snapshot_is_replaced_not_accumulated() {
    let pool = pool().await;
    let workspace = workspace_id("snapshot");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    assert!(
        storage
            .load_session_snapshot("chat")
            .await
            .unwrap()
            .is_none()
    );

    for thread in ["first-thread", "second-thread"] {
        storage
            .save_session_snapshot(
                "chat".to_string(),
                StoredRuntimeSnapshot {
                    drained_envelopes: Default::default(),
                    agent_drained_envelopes: Default::default(),
                    active_threads: [("explore".to_string(), thread.to_string())].into(),
                },
            )
            .await
            .unwrap();
    }

    let loaded = storage
        .load_session_snapshot("chat")
        .await
        .unwrap()
        .expect("a snapshot was saved");
    assert_eq!(
        loaded.active_threads.get("explore").map(String::as_str),
        Some("second-thread"),
        "the latest snapshot must win"
    );
    let rows = diesel::sql_query("select count(*) from runtime_snapshots where workspace_id = $1")
        .bind::<Text, _>(&workspace)
        .get_result::<CountRow>(&mut conn(&pool).await)
        .await
        .unwrap()
        .count;
    assert_eq!(rows, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_list_leads_with_the_most_recently_active_session() {
    let pool = pool().await;
    let workspace = workspace_id("list");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    storage
        .initialize_session("idle", test_binding())
        .await
        .unwrap();
    // `updated_at_ms` has millisecond resolution, so give the two sessions
    // distinguishable activity instead of relying on how fast the test runs.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    storage
        .initialize_session("active", test_binding())
        .await
        .unwrap();

    let turn = TurnId::from(MessageId::new());
    storage
        .session("active")
        .save_checkpoint(
            "active".to_string(),
            checkpoint(
                "active",
                vec![
                    entry(
                        turn,
                        Message::User(UserMessage::text(MessageId::new(), "recent session")),
                    ),
                    entry(
                        turn,
                        Message::User(UserMessage::text(MessageId::new(), "a later turn")),
                    ),
                ],
            ),
        )
        .await
        .unwrap();

    let sessions = storage.list_sessions().await.unwrap();

    assert_eq!(
        sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["active", "idle"]
    );
    assert!(sessions[0].updated_at_ms > sessions[1].updated_at_ms);
    assert_eq!(
        sessions[0].first_user_message.as_deref(),
        Some("recent session"),
        "the preview is the session's first user turn, not its latest"
    );
    assert!(!sessions[0].has_pending_approval);
    assert_eq!(sessions[1].first_user_message, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_image_only_first_turn_previews_as_a_placeholder() {
    let pool = pool().await;
    let workspace = workspace_id("images");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    storage
        .initialize_session("images", test_binding())
        .await
        .unwrap();

    storage
        .session("images")
        .save_checkpoint(
            "images".to_string(),
            checkpoint(
                "images",
                vec![entry(
                    TurnId::from(MessageId::new()),
                    Message::User(UserMessage::with_images(
                        MessageId::new(),
                        "",
                        &["data:image/png;base64,AAAA".to_string()],
                    )),
                )],
            ),
        )
        .await
        .unwrap();

    let sessions = storage.list_sessions().await.unwrap();
    assert_eq!(sessions[0].first_user_message.as_deref(), Some("[image]"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_list_flags_a_session_waiting_on_a_human() {
    let pool = pool().await;
    let workspace = workspace_id("approval");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    storage
        .initialize_session("review", test_binding())
        .await
        .unwrap();
    let session = storage.session("review");

    session
        .save_checkpoint("review".to_string(), checkpoint("review", vec![]))
        .await
        .unwrap();
    // The thread that is actually waiting is a sub-agent's, not the root's. Any
    // thread of the session counts, so the flag doesn't depend on which thread
    // happens to be suspended.
    session
        .save_checkpoint(
            "explore-thread".to_string(),
            StoredCheckpoint {
                resume_point: StoredResumePoint::PendingApproval {
                    parent_message_id: MessageId::new(),
                    pending_approval_calls: vec![ToolCall {
                        id: "call_shell".to_string(),
                        name: "shell".to_string(),
                        arguments: Some(r#"{"command":"cargo test"}"#.to_string()),
                    }],
                    pending_calls: vec![],
                },
                ..checkpoint("explore-thread", vec![])
            },
        )
        .await
        .unwrap();

    let sessions = storage.list_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].has_pending_approval);
}

#[tokio::test(flavor = "multi_thread")]
async fn reopening_a_session_keeps_the_binding_it_was_created_with() {
    let pool = pool().await;
    let workspace = workspace_id("binding");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);

    let created = storage
        .initialize_session("session-1", test_binding())
        .await
        .unwrap();
    assert_eq!(created, test_binding());

    // A browser that now prefers a different model must not silently move an
    // existing session onto it.
    let reopened = storage
        .initialize_session(
            "session-1",
            SessionModelBinding {
                provider_id: "other".to_string(),
                model_id: "different".to_string(),
                reasoning_effort: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(reopened, test_binding());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_name_can_be_set_and_cleared_without_touching_its_binding() {
    let pool = pool().await;
    let workspace = workspace_id("rename");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    storage
        .initialize_session("session-1", test_binding())
        .await
        .unwrap();

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

    let binding = storage
        .update_reasoning_effort("session-1", "openrouter", "x-ai/grok-4.5", Some("low"))
        .await
        .unwrap();
    assert_eq!(binding.reasoning_effort.as_deref(), Some("low"));
    assert_eq!(
        storage.list_sessions().await.unwrap()[0].name.as_deref(),
        Some("Investigation"),
        "changing the effort must not disturb the name"
    );

    assert_eq!(
        storage
            .rename_session("session-1", Some(" "))
            .await
            .unwrap(),
        None
    );
    assert_eq!(storage.list_sessions().await.unwrap()[0].name, None);
    assert_eq!(
        storage
            .initialize_session("session-1", test_binding())
            .await
            .unwrap()
            .reasoning_effort
            .as_deref(),
        Some("low"),
        "clearing the name must not disturb the effort"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn clearing_the_reasoning_effort_stores_a_json_null() {
    let pool = pool().await;
    let workspace = workspace_id("effort-null");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    storage
        .initialize_session("session-1", test_binding())
        .await
        .unwrap();

    let binding = storage
        .update_reasoning_effort("session-1", "openrouter", "x-ai/grok-4.5", None)
        .await
        .unwrap();

    assert_eq!(binding.reasoning_effort, None);
    // A missing key would deserialize the same way, but writing `null` keeps the
    // stored shape identical to what serde produces for `Option<String>`.
    let stored = diesel::sql_query(
        "select (model_binding->'reasoning_effort' #>> '{}' is null and
                 model_binding ? 'reasoning_effort') as ok
           from sessions where workspace_id = $1",
    )
    .bind::<Text, _>(&workspace)
    .get_result::<BoolRow>(&mut conn(&pool).await)
    .await
    .unwrap()
    .ok;
    assert!(stored);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_effort_update_for_a_different_model_is_rejected() {
    let pool = pool().await;
    let workspace = workspace_id("mismatch");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    storage
        .initialize_session("session-1", test_binding())
        .await
        .unwrap();

    assert_eq!(
        storage
            .update_reasoning_effort("session-1", "openrouter", "moonshotai/kimi-k3", Some("low"))
            .await,
        Err(SessionMetadataError::BindingMismatch)
    );
    assert_eq!(
        storage
            .update_reasoning_effort("missing", "openrouter", "x-ai/grok-4.5", Some("low"))
            .await,
        Err(SessionMetadataError::SessionNotFound),
        "a mismatch and a missing session are different answers"
    );
    assert_eq!(
        storage
            .initialize_session("session-1", test_binding())
            .await
            .unwrap(),
        test_binding(),
        "a rejected update must leave the binding alone"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn renaming_does_not_create_a_missing_session() {
    let pool = pool().await;
    let workspace = workspace_id("rename-missing");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);

    assert_eq!(
        storage.rename_session("missing", Some("name")).await,
        Err(RenameSessionError::SessionNotFound)
    );
    assert!(storage.list_sessions().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_session_leaves_the_list_and_is_reopenable() {
    let pool = pool().await;
    let workspace = workspace_id("delete");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    storage
        .initialize_session("doomed", test_binding())
        .await
        .unwrap();
    storage
        .session("doomed")
        .save_checkpoint(
            "doomed".to_string(),
            checkpoint(
                "doomed",
                vec![entry(
                    TurnId::from(MessageId::new()),
                    Message::User(UserMessage::text(MessageId::new(), "hello")),
                )],
            ),
        )
        .await
        .unwrap();

    storage.delete_session("doomed").await.unwrap();
    assert!(storage.list_sessions().await.unwrap().is_empty());
    // Deleting an already-deleted session is not an error: the old backend
    // treated a missing directory the same way.
    storage.delete_session("doomed").await.unwrap();

    // The id is free again, and nothing of the old session is left behind.
    storage
        .initialize_session("doomed", test_binding())
        .await
        .unwrap();
    assert_eq!(
        storage
            .session("doomed")
            .load_checkpoint("doomed")
            .await
            .unwrap()
            .map(|checkpoint| checkpoint.messages.len()),
        None
    );
}
