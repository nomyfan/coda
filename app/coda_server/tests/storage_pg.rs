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

use coda_agent::ThreadId;
use coda_agent::agent::{EnvelopeBody, Receiver, ReplyTarget};
use coda_agent::persist::{
    StateEntry, StoredCheckpoint, StoredPreparedToolCall, StoredResumePoint, StoredRuntimeSnapshot,
};
use coda_agent::runtime::SessionStorage;
use coda_agent::{Envelope, HistoryEntry, Sender};
use coda_core::llm::{
    AssistantMessage, CompactionMessage, CompactionOutcome, Message, MessageId, MessageOrigin,
    ReasoningContinuation, ToolCall, ToolCallOutcome, ToolMessage, ToolOutput, TurnId, UserMessage,
};
use coda_server::storage::DbPool;
use coda_server::storage::{
    CompactionError, ForkCut, ForkError, ForkSource, PgSessionStorage, RenameSessionError,
    RewindError, SessionMetadataError, SessionModelBinding, UnseenOutcome, WorkspaceStorage,
};
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
        state: vec![],
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

/// The summary a compaction writes, covering through `cutoff`.
fn summary_message(cutoff: MessageId, content: &str) -> Message {
    Message::Compaction(CompactionMessage {
        message_id: MessageId::new(),
        outcome: CompactionOutcome::Summary { cutoff },
        content: content.to_string(),
        created_at: jiff::Timestamp::default(),
    })
}

/// A task that reached the agent's inbox but not its history — what a snapshot
/// holds when the process stops between the two.
fn queued_task(thread_id: &str, task: &str) -> Envelope {
    Envelope::with_id(|id| Envelope {
        id,
        from: Sender::User,
        to: Receiver {
            name: "coda".to_string(),
            thread_id: ThreadId::from(thread_id.to_string()),
        },
        reply_to: None,
        body: EnvelopeBody::Task {
            message_id: MessageId::new(),
            task: task.to_string(),
            images: vec![],
        },
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
                 suspended_at, message_count, pending_approval)
             values ($1, $2, $2, 'coda', '\"Generation\"'::jsonb, now(), 1, false)",
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
             suspended_at, message_count, pending_approval)
         values ($1, 'never-opened', 'never-opened', 'coda', '\"Generation\"'::jsonb,
                 now(), 0, false)",
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
        state: vec![],
        resume_point: StoredResumePoint::PendingApproval {
            parent_message_id: MessageId::new(),
            pending_approval_calls: vec![StoredPreparedToolCall {
                tool_call: ToolCall {
                    id: "call_shell".to_string(),
                    name: "shell".to_string(),
                    arguments: Some(r#"{"command":"ls"}"#.to_string()),
                },
                metadata: None,
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

/// A compaction message rides the same row shape as any other, under its own
/// role. The role only has to be distinct — nothing reads it back to
/// reconstruct the message, and the `role = 'user'` filters that pick turn
/// boundaries and rewind targets must not match it.
#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_message_round_trips_under_its_own_role() {
    let pool = pool().await;
    let workspace = workspace_id("compaction-role");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let turn = TurnId::from(MessageId::new());
    let covered = Message::User(UserMessage::text(MessageId::new(), "hi"));
    let cutoff = covered.message_id();
    storage
        .save_checkpoint(
            "chat".to_string(),
            checkpoint(
                "chat",
                vec![
                    entry(turn, covered),
                    entry(
                        turn,
                        Message::Compaction(CompactionMessage {
                            message_id: MessageId::new(),
                            outcome: CompactionOutcome::Summary { cutoff },
                            content: "everything so far, in one paragraph".to_string(),
                            created_at: jiff::Timestamp::now(),
                        }),
                    ),
                ],
            ),
        )
        .await
        .unwrap();

    let loaded = storage.load_checkpoint("chat").await.unwrap().unwrap();
    let Message::Compaction(message) = &loaded.messages[1].message else {
        panic!("expected a compaction message");
    };
    assert_eq!(message.outcome, CompactionOutcome::Summary { cutoff });
    assert_eq!(message.content, "everything so far, in one paragraph");

    let user_rows = diesel::sql_query(
        "select count(*) from messages where workspace_id = $1 and role = 'user'",
    )
    .bind::<Text, _>(&workspace)
    .get_result::<CountRow>(&mut conn(&pool).await)
    .await
    .unwrap()
    .count;
    assert_eq!(
        user_rows, 1,
        "a compaction message must not read as a user turn"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_appends_its_two_messages_and_moves_the_watermark() {
    let pool = pool().await;
    let workspace = workspace_id("compact-commit");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let turn = TurnId::from(MessageId::new());
    storage
        .save_checkpoint(
            "chat".to_string(),
            checkpoint(
                "chat",
                vec![
                    entry(
                        turn,
                        Message::User(UserMessage::text(MessageId::new(), "do the thing")),
                    ),
                    entry(turn, assistant("done")),
                ],
            ),
        )
        .await
        .unwrap();

    let compaction_turn = TurnId::from(MessageId::new());
    let command = Message::User(UserMessage::text(
        MessageId::new(),
        "/compact keep the decisions",
    ));
    let summary = summary_message(command.message_id(), "we did the thing");
    storage
        .commit_compaction(2, compaction_turn, [&command, &summary])
        .await
        .unwrap();

    let loaded = storage.load_checkpoint("chat").await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 4);
    assert!(matches!(&loaded.messages[3].message, Message::Compaction(c) if c.is_summary()));

    // The watermark has to move with the rows: a later save starts its seqs
    // from it, and a stale one would collide with what was just written.
    storage
        .save_checkpoint(
            "chat".to_string(),
            checkpoint(
                "chat",
                loaded
                    .messages
                    .into_iter()
                    .chain([entry(compaction_turn, assistant("carrying on"))])
                    .collect(),
            ),
        )
        .await
        .expect("a normal save after a compaction must not collide");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_whose_thread_moved_on_writes_nothing() {
    let pool = pool().await;
    let workspace = workspace_id("compact-stale");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let turn = TurnId::from(MessageId::new());
    let opening = vec![entry(
        turn,
        Message::User(UserMessage::text(MessageId::new(), "do the thing")),
    )];
    storage
        .save_checkpoint("chat".to_string(), checkpoint("chat", opening.clone()))
        .await
        .unwrap();

    // What a compaction reads before it goes off to build a summary.
    let baseline = 1;

    // Meanwhile the session was released, reopened and given another turn.
    storage
        .save_checkpoint(
            "chat".to_string(),
            checkpoint(
                "chat",
                opening
                    .into_iter()
                    .chain([entry(turn, assistant("a reply the summary never saw"))])
                    .collect(),
            ),
        )
        .await
        .unwrap();

    let compaction_turn = TurnId::from(MessageId::new());
    let command = Message::User(UserMessage::text(MessageId::new(), "/compact"));
    let summary = summary_message(command.message_id(), "a summary of one message");
    let refused = storage
        .commit_compaction(baseline, compaction_turn, [&command, &summary])
        .await;

    assert!(matches!(refused, Err(CompactionError::Stale)));
    let loaded = storage.load_checkpoint("chat").await.unwrap().unwrap();
    assert_eq!(
        loaded.messages.len(),
        2,
        "a stale compaction must leave the thread exactly as it found it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_into_a_deleted_session_writes_nothing() {
    let pool = pool().await;
    let workspace = workspace_id("compact-deleted");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let turn = TurnId::from(MessageId::new());
    storage
        .save_checkpoint(
            "chat".to_string(),
            checkpoint(
                "chat",
                vec![entry(
                    turn,
                    Message::User(UserMessage::text(MessageId::new(), "do the thing")),
                )],
            ),
        )
        .await
        .unwrap();

    WorkspaceStorage::new(pool.clone(), &workspace)
        .delete_session("chat")
        .await
        .unwrap();

    let command = Message::User(UserMessage::text(MessageId::new(), "/compact"));
    let summary = summary_message(command.message_id(), "a summary of a session that is gone");
    let refused = storage
        .commit_compaction(1, TurnId::from(MessageId::new()), [&command, &summary])
        .await;

    assert!(matches!(refused, Err(CompactionError::Stale)));
    assert_eq!(row_count(&pool, "messages", &workspace).await, 0);
}

/// A binary `read_file`, or a `shell` command that wrote raw bytes, lands a NUL
/// in the tool result: `from_utf8_lossy` only rewrites *invalid* UTF-8, and NUL
/// is valid. PostgreSQL used to reject the whole checkpoint over it.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_carrying_a_nul_byte_still_saves() {
    let pool = pool().await;
    let workspace = workspace_id("nul-byte");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let turn = TurnId::from(MessageId::new());
    storage
        .save_checkpoint(
            "chat".to_string(),
            checkpoint(
                "chat",
                vec![
                    entry(
                        turn,
                        Message::User(UserMessage::text(MessageId::new(), "read the header")),
                    ),
                    entry(
                        turn,
                        Message::Tool(ToolMessage::new(
                            "call_read",
                            "read_file",
                            ToolOutput::Ok("\u{0}ELF\u{0}\u{0}stripped".to_string()),
                            ToolCallOutcome::Auto,
                            None,
                        )),
                    ),
                ],
            ),
        )
        .await
        .expect("a NUL in a tool result must not cost the whole checkpoint");

    let loaded = storage.load_checkpoint("chat").await.unwrap().unwrap();
    let Message::Tool(message) = &loaded.messages[1].message else {
        panic!("expected a tool message");
    };
    let ToolOutput::Ok(output) = &message.output else {
        panic!("expected a successful tool result");
    };
    // Every other byte survives; the NULs come back as U+FFFD.
    assert_eq!(output, "\u{fffd}ELF\u{fffd}\u{fffd}stripped");
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
                state: vec![],
                resume_point: StoredResumePoint::PendingApproval {
                    parent_message_id: MessageId::new(),
                    pending_approval_calls: vec![StoredPreparedToolCall {
                        tool_call: ToolCall {
                            id: "call_shell".to_string(),
                            name: "shell".to_string(),
                            arguments: Some(r#"{"command":"cargo test"}"#.to_string()),
                        },
                        metadata: None,
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
async fn an_unseen_outcome_can_be_marked_and_cleared() {
    let pool = pool().await;
    let workspace = workspace_id("unseen");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    storage
        .initialize_session("session-1", test_binding())
        .await
        .unwrap();
    assert_eq!(
        storage.list_sessions().await.unwrap()[0].unseen_outcome,
        None
    );

    storage
        .mark_unseen_outcome("session-1", UnseenOutcome::Failed)
        .await
        .unwrap();
    assert_eq!(
        storage.list_sessions().await.unwrap()[0]
            .unseen_outcome
            .as_deref(),
        Some("failed")
    );

    storage.clear_unseen_outcome("session-1").await.unwrap();
    assert_eq!(
        storage.list_sessions().await.unwrap()[0].unseen_outcome,
        None
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn clearing_an_unseen_outcome_that_was_never_set_is_a_no_op() {
    let pool = pool().await;
    let workspace = workspace_id("unseen-noop");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    storage
        .initialize_session("session-1", test_binding())
        .await
        .unwrap();

    storage.clear_unseen_outcome("session-1").await.unwrap();
    assert_eq!(
        storage.list_sessions().await.unwrap()[0].unseen_outcome,
        None
    );
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

// --- rewind ------------------------------------------------------------------

#[derive(QueryableByName)]
struct ThreadCountRow {
    #[diesel(sql_type = Text)]
    thread_id: String,
    #[diesel(sql_type = Integer)]
    message_count: i32,
}

/// A three-thread session spanning two turns, built the way the runtime builds
/// one: a root thread, a stateful sub-agent called in both turns, and a
/// stateless sub-agent called only in the second. Returns the two turns and the
/// root user message that opened each.
async fn seed_two_turn_session(
    pool: &DbPool,
    workspace: &str,
) -> (TurnId, TurnId, MessageId, MessageId) {
    seed_session(pool, workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), workspace, "chat");

    let first_root = MessageId::new();
    let second_root = MessageId::new();
    let first = TurnId::from(first_root);
    let second = TurnId::from(second_root);

    // The assistant message each turn's sub-agent calls hang off; a stateless
    // thread's id is derived from it, which is why it cannot outlive it.
    let first_caller = MessageId::new();
    let second_caller = MessageId::new();

    storage
        .save_checkpoint(
            "chat".to_string(),
            checkpoint(
                "chat",
                vec![
                    entry(
                        first,
                        Message::User(UserMessage::text(first_root, "start here")),
                    ),
                    entry(first, assistant("asking explore")),
                    entry(
                        second,
                        Message::User(UserMessage::text(second_root, "now do this instead")),
                    ),
                    entry(second, assistant("asking explore and probe")),
                ],
            ),
        )
        .await
        .unwrap();

    // Stateful: one thread, reached by both turns.
    let explore_first = MessageOrigin {
        message_id: first_caller,
        call_id: "call_explore_1".to_string(),
    };
    let explore_second = MessageOrigin {
        message_id: second_caller,
        call_id: "call_explore_2".to_string(),
    };
    storage
        .save_checkpoint(
            "explore-thread".to_string(),
            StoredCheckpoint {
                agent_name: "explore".to_string(),
                parent_thread_id: Some("chat".to_string()),
                derivation_key: Some("explore".to_string()),
                ..checkpoint(
                    "explore-thread",
                    vec![
                        entry(
                            first,
                            Message::User(UserMessage::from_subagent_call(
                                MessageId::new(),
                                "look at the schema",
                                explore_first,
                            )),
                        ),
                        entry(first, assistant("four tables")),
                        entry(
                            second,
                            Message::User(UserMessage::from_subagent_call(
                                MessageId::new(),
                                "look again",
                                explore_second,
                            )),
                        ),
                        entry(second, assistant("still four")),
                    ],
                )
            },
        )
        .await
        .unwrap();

    // Stateless: its own thread, reached only by the second turn.
    let probe_call = MessageOrigin {
        message_id: second_caller,
        call_id: "call_probe".to_string(),
    };
    storage
        .save_checkpoint(
            "probe-thread".to_string(),
            StoredCheckpoint {
                agent_name: "probe".to_string(),
                parent_thread_id: Some("chat".to_string()),
                derivation_key: Some(probe_call.derivation_key()),
                ..checkpoint(
                    "probe-thread",
                    vec![
                        entry(
                            second,
                            Message::User(UserMessage::from_subagent_call(
                                MessageId::new(),
                                "check the index",
                                probe_call,
                            )),
                        ),
                        entry(second, assistant("it is used")),
                    ],
                )
            },
        )
        .await
        .unwrap();

    storage
        .save_session_snapshot(
            "chat".to_string(),
            StoredRuntimeSnapshot {
                drained_envelopes: Default::default(),
                agent_drained_envelopes: Default::default(),
                active_threads: Default::default(),
            },
        )
        .await
        .unwrap();

    (first, second, first_root, second_root)
}

async fn thread_ids_and_seqs(pool: &DbPool, workspace: &str) -> Vec<(String, i32)> {
    diesel::sql_query(
        "select thread_id, seq from messages where workspace_id = $1 order by thread_id, seq",
    )
    .bind::<Text, _>(workspace)
    .load::<ThreadSeqRow>(&mut conn(pool).await)
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.thread_id, row.seq))
    .collect()
}

async fn thread_counts(pool: &DbPool, workspace: &str) -> Vec<(String, i32)> {
    diesel::sql_query(
        "select thread_id, message_count from thread_checkpoints
          where workspace_id = $1 order by thread_id",
    )
    .bind::<Text, _>(workspace)
    .load::<ThreadCountRow>(&mut conn(pool).await)
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.thread_id, row.message_count))
    .collect()
}

async fn row_count(pool: &DbPool, table: &str, workspace: &str) -> i64 {
    diesel::sql_query(format!(
        "select count(*) as count from {table} where workspace_id = $1"
    ))
    .bind::<Text, _>(workspace)
    .get_result::<CountRow>(&mut conn(pool).await)
    .await
    .unwrap()
    .count
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_drops_the_discarded_turn_from_every_thread_it_reached() {
    let pool = pool().await;
    let workspace = workspace_id("rewind-across-threads");
    let (_, _, _, second_root) = seed_two_turn_session(&pool, &workspace).await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let remaining = storage.rewind_to(second_root).await.unwrap();

    // What comes back is the root thread's conversation, which is what the
    // client renders — the sub-agent threads are truncated but never shown.
    assert_eq!(remaining.len(), 2);
    assert!(
        matches!(&remaining[0], Message::User(user) if user.first_text() == Some("start here")),
        "the surviving history must start at the first turn"
    );

    // The second turn is gone from every thread it reached, and only from those:
    // the stateful thread keeps the turn that came before it, the stateless
    // thread had nothing else and its row goes.
    assert_eq!(
        thread_ids_and_seqs(&pool, &workspace).await,
        vec![
            ("chat".to_string(), 0),
            ("chat".to_string(), 1),
            ("explore-thread".to_string(), 0),
            ("explore-thread".to_string(), 1),
        ]
    );
    assert_eq!(
        thread_counts(&pool, &workspace).await,
        vec![("chat".to_string(), 2), ("explore-thread".to_string(), 2)],
        "an emptied thread is removed and the survivors' watermarks come down"
    );
    assert_eq!(
        row_count(&pool, "runtime_snapshots", &workspace).await,
        0,
        "queued envelopes belong to the discarded turn; keeping them would replay it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewound_thread_keeps_growing_from_where_it_was_cut() {
    let pool = pool().await;
    let workspace = workspace_id("rewind-watermark");
    let (first, _, _, second_root) = seed_two_turn_session(&pool, &workspace).await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let remaining = storage.rewind_to(second_root).await.unwrap();

    // The turn a rewind starts writes into the thread it just shortened. If the
    // watermark stayed where it was, this append would land past the end of the
    // history and be silently dropped.
    let replacement = MessageId::new();
    let mut history: Vec<HistoryEntry> = remaining
        .into_iter()
        .map(|message| entry(first, message))
        .collect();
    let replacement_turn = TurnId::from(replacement);
    history.push(entry(
        replacement_turn,
        Message::User(UserMessage::text(replacement, "try this instead")),
    ));
    history.push(entry(replacement_turn, assistant("done")));
    storage
        .save_checkpoint("chat".to_string(), checkpoint("chat", history))
        .await
        .unwrap();

    let reloaded = storage
        .load_checkpoint("chat")
        .await
        .unwrap()
        .expect("the root thread was just saved");
    let texts: Vec<String> = reloaded
        .messages
        .iter()
        .map(|entry| match &entry.message {
            Message::User(user) => user.first_text().unwrap_or_default().to_string(),
            Message::Assistant(assistant) => assistant.content.clone(),
            other => panic!("unexpected message {other:?}"),
        })
        .collect();
    assert_eq!(
        texts,
        vec!["start here", "asking explore", "try this instead", "done"],
        "the replacement turn must follow the surviving history exactly once"
    );
    assert_eq!(
        thread_ids_and_seqs(&pool, &workspace)
            .await
            .into_iter()
            .filter(|(thread_id, _)| thread_id == "chat")
            .map(|(_, seq)| seq)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3],
        "seq stays a contiguous run so later loads keep their order"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rewinding_to_the_opening_message_leaves_no_session_state_behind() {
    let pool = pool().await;
    let workspace = workspace_id("rewind-to-start");
    let (_, _, first_root, _) = seed_two_turn_session(&pool, &workspace).await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    assert!(storage.rewind_to(first_root).await.unwrap().is_empty());

    // Every thread is emptied, so every thread record goes with it — including
    // the root's. The session itself survives and reopens as a blank one.
    assert_eq!(row_count(&pool, "messages", &workspace).await, 0);
    assert_eq!(row_count(&pool, "thread_checkpoints", &workspace).await, 0);
    assert_eq!(row_count(&pool, "sessions", &workspace).await, 1);
    assert!(storage.load_checkpoint("chat").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_is_refused_while_any_thread_is_mid_turn() {
    let pool = pool().await;
    let workspace = workspace_id("rewind-busy");
    let (_, _, _, second_root) = seed_two_turn_session(&pool, &workspace).await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");
    let before = thread_ids_and_seqs(&pool, &workspace).await;

    // A sub-agent waiting on an approval, which the `pending_approval` column
    // would flag — and one waiting on a tool result, which it would not. Both
    // must block a rewind, which is why the check reads `resume_point` itself.
    for (parked, flagged) in [
        (
            StoredResumePoint::PendingApproval {
                parent_message_id: MessageId::new(),
                pending_approval_calls: vec![StoredPreparedToolCall {
                    tool_call: ToolCall {
                        id: "call_shell".to_string(),
                        name: "shell".to_string(),
                        arguments: None,
                    },
                    metadata: None,
                }],
                pending_calls: vec![],
            },
            true,
        ),
        (
            StoredResumePoint::ToolExecution(coda_agent::persist::StoredToolExecutionState {
                parent_message_id: MessageId::new(),
                pending_replies: vec![],
                tool_calls: vec![],
            }),
            false,
        ),
    ] {
        // Both columns move together, exactly as `save_checkpoint` writes them —
        // so the second case really is a thread the `pending_approval` flag does
        // not mark, rather than one whose fixture merely forgot to set it.
        diesel::sql_query(
            "update thread_checkpoints set resume_point = $2, pending_approval = $3
              where workspace_id = $1 and thread_id = 'explore-thread'",
        )
        .bind::<Text, _>(&workspace)
        .bind::<diesel::sql_types::Jsonb, _>(serde_json::to_value(&parked).unwrap())
        .bind::<Bool, _>(flagged)
        .execute(&mut conn(&pool).await)
        .await
        .unwrap();

        assert_eq!(
            storage.rewind_to(second_root).await.unwrap_err(),
            RewindError::ThreadBusy {
                thread_id: "explore-thread".to_string()
            }
        );
        assert_eq!(
            thread_ids_and_seqs(&pool, &workspace).await,
            before,
            "a refused rewind must not have deleted anything"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_user_message_of_the_root_thread_can_be_rewound_to() {
    let pool = pool().await;
    let workspace = workspace_id("rewind-target");
    let (_, _, _, _) = seed_two_turn_session(&pool, &workspace).await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");
    let before = thread_ids_and_seqs(&pool, &workspace).await;

    let root = storage.load_checkpoint("chat").await.unwrap().unwrap();
    let assistant_id = match &root.messages[1].message {
        Message::Assistant(message) => message.message_id,
        other => panic!("expected an assistant message, got {other:?}"),
    };
    let sub = storage
        .load_checkpoint("explore-thread")
        .await
        .unwrap()
        .unwrap();
    let sub_user_id = match &sub.messages[0].message {
        Message::User(message) => message.message_id,
        other => panic!("expected a user message, got {other:?}"),
    };

    // An assistant message of the root thread, a user message of a sub-agent
    // thread, and an id from nowhere: each names something real (or plausible)
    // that still is not a rewind target.
    for target in [assistant_id, sub_user_id, MessageId::new()] {
        assert_eq!(
            storage.rewind_to(target).await.unwrap_err(),
            RewindError::TargetNotFound
        );
    }
    assert_eq!(thread_ids_and_seqs(&pool, &workspace).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_truncation_that_would_leave_a_gap_is_rolled_back() {
    let pool = pool().await;
    let workspace = workspace_id("rewind-gap");
    seed_session(&pool, &workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "chat");

    let first = TurnId::from(MessageId::new());
    let second_root = MessageId::new();
    let second = TurnId::from(second_root);
    storage
        .save_checkpoint(
            "chat".to_string(),
            checkpoint(
                "chat",
                vec![
                    entry(
                        first,
                        Message::User(UserMessage::text(MessageId::new(), "first")),
                    ),
                    entry(first, assistant("ok")),
                    entry(
                        second,
                        Message::User(UserMessage::text(second_root, "second")),
                    ),
                ],
            ),
        )
        .await
        .unwrap();

    // A sub-agent thread whose turns are *not* in contiguous blocks. Production
    // cannot produce this — envelopes are handled one at a time and in order —
    // so it is written directly; the point is that if that ever stopped holding,
    // the truncation would punch a hole rather than take a tail.
    let mut conn = conn(&pool).await;
    diesel::sql_query(
        "insert into thread_checkpoints
            (workspace_id, session_id, thread_id, agent_name, resume_point,
             suspended_at, message_count, pending_approval)
         values ($1, 'chat', 'interleaved', 'explore', '\"Generation\"'::jsonb,
                 now(), 3, false)",
    )
    .bind::<Text, _>(&workspace)
    .execute(&mut conn)
    .await
    .unwrap();
    for (seq, turn) in [(0, first), (1, second), (2, first)] {
        diesel::sql_query(
            "insert into messages
                (workspace_id, session_id, thread_id, seq, message_id, turn_id, role, payload)
             values ($1, 'chat', 'interleaved', $2, gen_random_uuid(), $3, 'assistant', '{}'::jsonb)",
        )
        .bind::<Text, _>(&workspace)
        .bind::<Integer, _>(seq)
        .bind::<diesel::sql_types::Uuid, _>(turn.as_uuid())
        .execute(&mut conn)
        .await
        .unwrap();
    }
    let before = thread_ids_and_seqs(&pool, &workspace).await;

    assert_eq!(
        storage.rewind_to(second_root).await.unwrap_err(),
        RewindError::HistoryNotContiguous {
            thread_id: "interleaved".to_string()
        }
    );
    assert_eq!(
        thread_ids_and_seqs(&pool, &workspace).await,
        before,
        "the whole transaction rolls back, including the root thread's deletions"
    );
}

/// The workspace's session ids, ordered.
async fn sessions_in(pool: &DbPool, workspace: &str) -> Vec<String> {
    diesel::sql_query("select session_id from sessions where workspace_id = $1 order by session_id")
        .bind::<Text, _>(workspace)
        .load::<SessionIdRow>(&mut conn(pool).await)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.session_id)
        .collect()
}

/// Every thread of one session with its message count, ordered by id.
async fn threads_of(pool: &DbPool, workspace: &str, session: &str) -> Vec<(String, i32)> {
    diesel::sql_query(
        "select thread_id, message_count from thread_checkpoints
          where workspace_id = $1 and session_id = $2 order by thread_id",
    )
    .bind::<Text, _>(workspace)
    .bind::<Text, _>(session)
    .load::<ThreadCountRow>(&mut conn(pool).await)
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.thread_id, row.message_count))
    .collect()
}

/// One session's messages as `(thread_id, seq)`, ordered.
async fn messages_of(pool: &DbPool, workspace: &str, session: &str) -> Vec<(String, i32)> {
    diesel::sql_query(
        "select thread_id, seq from messages
          where workspace_id = $1 and session_id = $2 order by thread_id, seq",
    )
    .bind::<Text, _>(workspace)
    .bind::<Text, _>(session)
    .load::<ThreadSeqRow>(&mut conn(pool).await)
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.thread_id, row.seq))
    .collect()
}

/// Rows of `session` that mention `needle` anywhere, jsonb included. Casting the
/// whole row to text is what makes this catch a thread id hiding in a column the
/// fork does not know about yet.
async fn rows_mentioning(pool: &DbPool, workspace: &str, session: &str, needle: &str) -> i64 {
    diesel::sql_query(
        "select (select count(*) from messages m
                  where m.workspace_id = $1 and m.session_id = $2
                    and m::text like '%' || $3 || '%')
              + (select count(*) from thread_checkpoints t
                  where t.workspace_id = $1 and t.session_id = $2
                    and t::text like '%' || $3 || '%') as count",
    )
    .bind::<Text, _>(workspace)
    .bind::<Text, _>(session)
    .bind::<Text, _>(needle)
    .get_result::<CountRow>(&mut conn(pool).await)
    .await
    .unwrap()
    .count
}

/// A session with a stateful sub-agent across three turns, forked at the second.
///
/// This is the design's load-bearing pair: thread ids are rebuilt under the new
/// root, and the retained turns leave every thread a contiguous prefix.
#[tokio::test(flavor = "multi_thread")]
async fn a_fork_rebuilds_thread_ids_and_keeps_each_thread_a_prefix() {
    let pool = pool().await;
    let workspace = workspace_id("fork-remap");
    seed_session(&pool, &workspace, "source-session").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "source-session");
    let explore = ThreadId::from_uuid5(&ThreadId::from("source-session".to_string()), "explore");

    let (first, second, third) = (
        TurnId::from(MessageId::new()),
        TurnId::from(MessageId::new()),
        TurnId::from(MessageId::new()),
    );
    // The turn to branch away from: everything before it is kept, it and the
    // rest are not.
    let branch_point = Message::User(UserMessage::text(MessageId::new(), "q3"));
    let cut = branch_point.message_id();
    storage
        .save_checkpoint(
            "source-session".to_string(),
            checkpoint(
                "source-session",
                vec![
                    entry(
                        first,
                        Message::User(UserMessage::text(MessageId::new(), "q1")),
                    ),
                    entry(first, assistant("a1")),
                    entry(
                        second,
                        Message::User(UserMessage::text(MessageId::new(), "q2")),
                    ),
                    entry(second, assistant("delegating")),
                    entry(second, assistant("the answer worth branching from")),
                    entry(third, branch_point),
                    entry(third, assistant("a3")),
                ],
            ),
        )
        .await
        .unwrap();

    // The sub-agent keeps one thread across both calls, so the fork has to cut
    // its history too — by turn, not by its own seq.
    storage
        .save_checkpoint(
            explore.as_ref().to_string(),
            StoredCheckpoint {
                thread_id: explore.as_ref().to_string(),
                agent_name: "explore".to_string(),
                parent_thread_id: Some("source-session".to_string()),
                derivation_key: Some("explore".to_string()),
                reply_target: None,
                messages: vec![
                    entry(
                        second,
                        Message::User(UserMessage::text(MessageId::new(), "look")),
                    ),
                    entry(second, assistant("found")),
                    entry(
                        third,
                        Message::User(UserMessage::text(MessageId::new(), "again")),
                    ),
                    entry(third, assistant("found again")),
                ],
                state: vec![],
                resume_point: StoredResumePoint::Generation,
                suspended_at: jiff::Timestamp::default(),
            },
        )
        .await
        .unwrap();

    let forked = WorkspaceStorage::new(pool.clone(), &workspace)
        .fork_session("source-session", ForkCut::At(cut), ForkSource::Cold)
        .await
        .unwrap();

    let new_explore = ThreadId::from_uuid5(&ThreadId::from(forked.session_id.clone()), "explore");
    let mut expected = vec![
        (forked.session_id.clone(), 5),
        (new_explore.as_ref().to_string(), 2),
    ];
    expected.sort();
    assert_eq!(
        threads_of(&pool, &workspace, &forked.session_id).await,
        expected,
        "the root thread takes the new session id and the child is re-derived from it"
    );

    let mut expected_messages: Vec<(String, i32)> = (0..5)
        .map(|seq| (forked.session_id.clone(), seq))
        .chain((0..2).map(|seq| (new_explore.as_ref().to_string(), seq)))
        .collect();
    expected_messages.sort();
    assert_eq!(
        messages_of(&pool, &workspace, &forked.session_id).await,
        expected_messages,
        "each thread keeps a contiguous prefix, so `message_count` stays the next free seq"
    );

    assert_eq!(
        rows_mentioning(&pool, &workspace, &forked.session_id, "source-session").await,
        0,
        "no row of the fork may still name the source session"
    );
    assert_eq!(
        rows_mentioning(&pool, &workspace, &forked.session_id, explore.as_ref()).await,
        0,
        "nor the source's derived thread id, jsonb included"
    );

    assert_eq!(
        messages_of(&pool, &workspace, "source-session").await.len(),
        11,
        "the source is only read"
    );
}

/// The cut names the turn to branch *away* from, so only the message that opens
/// one will do — rewind's rule exactly. Everything else lands on the same error,
/// because the database cannot tell a forged cut from one that simply has not
/// been stored yet.
#[tokio::test(flavor = "multi_thread")]
async fn only_a_user_message_of_the_root_thread_can_be_a_cut() {
    let pool = pool().await;
    let workspace = workspace_id("fork-cut");
    seed_session(&pool, &workspace, "source-session").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "source-session");
    let explore = ThreadId::from_uuid5(&ThreadId::from("source-session".to_string()), "explore");

    let (kept, dropped) = (
        TurnId::from(MessageId::new()),
        TurnId::from(MessageId::new()),
    );
    let reply = assistant("what the first turn answered");
    let sub_reply = assistant("what the sub-agent said");
    let branch_point = Message::User(UserMessage::text(
        MessageId::new(),
        "and now something else",
    ));
    let (cut, reply_cut, sub_agent_cut) = (
        branch_point.message_id(),
        reply.message_id(),
        sub_reply.message_id(),
    );

    storage
        .save_checkpoint(
            "source-session".to_string(),
            checkpoint(
                "source-session",
                vec![
                    entry(
                        kept,
                        Message::User(UserMessage::text(MessageId::new(), "q")),
                    ),
                    entry(kept, reply),
                    entry(dropped, branch_point),
                    entry(dropped, assistant("the answer being branched away from")),
                ],
            ),
        )
        .await
        .unwrap();
    storage
        .save_checkpoint(
            explore.as_ref().to_string(),
            StoredCheckpoint {
                thread_id: explore.as_ref().to_string(),
                agent_name: "explore".to_string(),
                parent_thread_id: Some("source-session".to_string()),
                derivation_key: Some("explore".to_string()),
                messages: vec![entry(kept, sub_reply)],
                ..checkpoint(explore.as_ref(), vec![])
            },
        )
        .await
        .unwrap();

    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    let forked = storage
        .fork_session("source-session", ForkCut::At(cut), ForkSource::Cold)
        .await
        .expect("the message that opened a turn");
    let new_explore = ThreadId::from_uuid5(&ThreadId::from(forked.session_id.clone()), "explore");
    let mut expected = vec![
        (forked.session_id.clone(), 2),
        (new_explore.as_ref().to_string(), 1),
    ];
    expected.sort();
    assert_eq!(
        threads_of(&pool, &workspace, &forked.session_id).await,
        expected,
        "the cut's own turn is dropped, on every thread it reached"
    );

    for (cut, why) in [
        (reply_cut, "an assistant reply does not open a turn"),
        (
            sub_agent_cut,
            "a sub-agent's message is not on the root thread",
        ),
        (MessageId::new(), "no such message at all"),
    ] {
        assert_eq!(
            storage
                .fork_session("source-session", ForkCut::At(cut), ForkSource::Cold)
                .await,
            Err(ForkError::CutNotFound),
            "{why}"
        );
    }
    assert_eq!(
        sessions_in(&pool, &workspace).await.len(),
        2,
        "the source plus the one accepted copy; a refused fork mints nothing"
    );
}

/// A thread parked anywhere but a plain generation boundary means the session is
/// mid-flight, and its stored state is not something to copy.
#[tokio::test(flavor = "multi_thread")]
async fn forking_a_session_with_work_in_flight_changes_nothing() {
    let pool = pool().await;
    let workspace = workspace_id("fork-busy");
    seed_session(&pool, &workspace, "source-session").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "source-session");

    let turn = TurnId::from(MessageId::new());
    // A valid cut, so what the fork trips over is the resting point and not the
    // cut lookup.
    let prompt = Message::User(UserMessage::text(MessageId::new(), "q"));
    let cut = prompt.message_id();
    storage
        .save_checkpoint(
            "source-session".to_string(),
            StoredCheckpoint {
                state: vec![],
                resume_point: StoredResumePoint::PendingApproval {
                    parent_message_id: MessageId::new(),
                    pending_approval_calls: vec![StoredPreparedToolCall {
                        tool_call: ToolCall {
                            id: "call_shell".to_string(),
                            name: "shell".to_string(),
                            arguments: Some(r#"{"command":"rm -rf /"}"#.to_string()),
                        },
                        metadata: None,
                    }],
                    pending_calls: vec![],
                },
                ..checkpoint(
                    "source-session",
                    vec![entry(turn, prompt), entry(turn, assistant("answered"))],
                )
            },
        )
        .await
        .unwrap();

    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    assert_eq!(
        storage
            .fork_session("source-session", ForkCut::At(cut), ForkSource::Cold)
            .await,
        Err(ForkError::ThreadBusy {
            thread_id: "source-session".to_string()
        })
    );
    assert_eq!(
        storage
            .fork_session("source-session", ForkCut::All, ForkSource::Cold)
            .await,
        Err(ForkError::ThreadBusy {
            thread_id: "source-session".to_string()
        }),
        "a full copy is held to the same resting point"
    );
    assert_eq!(
        sessions_in(&pool, &workspace).await,
        vec!["source-session".to_string()]
    );
}

/// The state a turn leaves behind the moment it starts: the driver checkpoints
/// the prompt on its own so a crash mid-turn still has it, and writes nothing
/// else until the turn ends. An abort settles the turn — clearing the relay's
/// `turn_running` — one event before that final write lands, so a fork can
/// arrive to find a turn that is all prefix and no body.
///
/// Cutting at the prompt is exactly what makes that harmless: the half-written
/// turn is the one turn the copy leaves behind, and seeing the prompt at all
/// proves everything before it committed.
#[tokio::test(flavor = "multi_thread")]
async fn a_cut_ignores_the_half_written_turn_it_opens() {
    let pool = pool().await;
    let workspace = workspace_id("fork-half-turn");
    seed_session(&pool, &workspace, "source-session").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "source-session");

    let (done, started) = (
        TurnId::from(MessageId::new()),
        TurnId::from(MessageId::new()),
    );
    let prompt = Message::User(UserMessage::text(MessageId::new(), "run the thing"));
    let cut = prompt.message_id();
    storage
        .save_checkpoint(
            "source-session".to_string(),
            checkpoint(
                "source-session",
                vec![
                    entry(
                        done,
                        Message::User(UserMessage::text(MessageId::new(), "q")),
                    ),
                    entry(done, assistant("a")),
                    entry(started, prompt),
                ],
            ),
        )
        .await
        .unwrap();

    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    let forked = storage
        .fork_session(
            "source-session",
            ForkCut::At(cut),
            // The relay has the tool call and its aborted result too; the
            // database has neither yet.
            ForkSource::Live,
        )
        .await
        .expect("what has not landed is in the turn being branched away from");
    assert_eq!(
        threads_of(&pool, &workspace, &forked.session_id).await,
        vec![(forked.session_id.clone(), 2)],
        "only the turn that finished comes across"
    );
}

/// Checkpoints alone don't prove a cold session is at rest: a task queued behind
/// the last turn survives shutdown in the runtime snapshot while every
/// checkpoint reads `Generation`, and the next open picks it back up.
#[tokio::test(flavor = "multi_thread")]
async fn forking_a_cold_session_with_queued_work_changes_nothing() {
    let pool = pool().await;
    let workspace = workspace_id("fork-queued");
    seed_session(&pool, &workspace, "source-session").await;
    let storage = PgSessionStorage::new(pool.clone(), &workspace, "source-session");

    let (first, second) = (
        TurnId::from(MessageId::new()),
        TurnId::from(MessageId::new()),
    );
    let branch_point = Message::User(UserMessage::text(MessageId::new(), "next"));
    let cut = branch_point.message_id();
    storage
        .save_checkpoint(
            "source-session".to_string(),
            checkpoint(
                "source-session",
                vec![
                    entry(
                        first,
                        Message::User(UserMessage::text(MessageId::new(), "q")),
                    ),
                    entry(first, assistant("answered")),
                    entry(second, branch_point),
                    entry(second, assistant("answered again")),
                ],
            ),
        )
        .await
        .unwrap();
    storage
        .save_session_snapshot(
            "source-session".to_string(),
            StoredRuntimeSnapshot {
                drained_envelopes: Default::default(),
                agent_drained_envelopes: [(
                    "coda".to_string(),
                    vec![queued_task("source-session", "and one more thing")],
                )]
                .into(),
                active_threads: Default::default(),
            },
        )
        .await
        .unwrap();

    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    assert_eq!(
        storage
            .fork_session("source-session", ForkCut::At(cut), ForkSource::Cold)
            .await,
        Err(ForkError::SourceNotIdle {
            thread_id: "source-session".to_string()
        })
    );
    assert_eq!(
        sessions_in(&pool, &workspace).await,
        vec!["source-session".to_string()],
        "a refused fork mints nothing"
    );

    // The same row means nothing while a runtime is attached: it is only
    // rewritten when an agent exits, so it describes the last shutdown, not now.
    storage
        .fork_session("source-session", ForkCut::At(cut), ForkSource::Live)
        .await
        .expect("a live source is judged by its checkpoints alone");
}

/// What a fork carries over from the source besides its messages.
#[tokio::test(flavor = "multi_thread")]
async fn a_fork_inherits_the_name_and_binding() {
    let pool = pool().await;
    let workspace = workspace_id("fork-inherit");
    seed_session(&pool, &workspace, "source-session").await;
    let workspace_storage = WorkspaceStorage::new(pool.clone(), &workspace);
    workspace_storage
        .rename_session("source-session", Some("worth branching"))
        .await
        .unwrap();

    let turn = TurnId::from(MessageId::new());
    PgSessionStorage::new(pool.clone(), &workspace, "source-session")
        .save_checkpoint(
            "source-session".to_string(),
            checkpoint(
                "source-session",
                vec![
                    entry(
                        turn,
                        Message::User(UserMessage::text(MessageId::new(), "q")),
                    ),
                    entry(turn, assistant("a")),
                ],
            ),
        )
        .await
        .unwrap();

    let forked = workspace_storage
        .fork_session("source-session", ForkCut::All, ForkSource::Cold)
        .await
        .unwrap();

    assert_eq!(
        forked.name.as_deref(),
        Some("worth branching"),
        "the name is copied as-is; no prefix is added"
    );
    let summaries = workspace_storage.list_sessions().await.unwrap();
    assert_eq!(
        summaries[0].session_id, forked.session_id,
        "the copy is the most recently touched session"
    );
    assert_eq!(summaries[0].name.as_deref(), Some("worth branching"));
}

#[tokio::test(flavor = "multi_thread")]
async fn forking_a_session_that_does_not_exist_is_refused() {
    let pool = pool().await;
    let workspace = workspace_id("fork-missing");

    assert_eq!(
        WorkspaceStorage::new(pool.clone(), &workspace)
            .fork_session("never-existed", ForkCut::All, ForkSource::Cold)
            .await,
        Err(ForkError::SourceNotFound)
    );
    assert!(sessions_in(&pool, &workspace).await.is_empty());
}

// ---------------------------------------------------------------------------
// Anchored thread state
// ---------------------------------------------------------------------------

/// A tool call and its result, plus the state the call recorded — the shape the
/// runtime produces. `kind` is opaque here on purpose: nothing in storage knows
/// what any kind holds, which is the whole point of the mechanism.
fn recorded(
    turn: TurnId,
    call_id: &str,
    kind: &str,
    value: serde_json::Value,
) -> (Vec<HistoryEntry>, StateEntry) {
    let result = ToolMessage::new(
        call_id.to_string(),
        "a_tool".to_string(),
        ToolOutput::Ok("done".to_string()),
        ToolCallOutcome::Auto,
        None,
    );
    let anchor = result.message_id;
    let call = Message::Assistant(AssistantMessage {
        message_id: MessageId::new(),
        content: String::new(),
        tool_calls: vec![ToolCall {
            id: call_id.to_string(),
            name: "a_tool".to_string(),
            arguments: None,
        }],
        usage: None,
        reasoning_content: None,
        reasoning_continuation: None,
        reasoning_ended_at: None,
        aborted: false,
        started_at: jiff::Timestamp::default(),
        ended_at: jiff::Timestamp::default(),
    });
    (
        vec![entry(turn, call), entry(turn, Message::Tool(result))],
        StateEntry {
            message_id: anchor,
            kind: kind.to_string(),
            value,
        },
    )
}

/// Two turns, each recording a different value of the same kind. Returns the
/// user message that opened the second turn — the cut a fork or a rewind takes.
async fn seed_session_with_state(pool: &DbPool, workspace: &str) -> MessageId {
    seed_session(pool, workspace, "chat").await;
    let storage = PgSessionStorage::new(pool.clone(), workspace, "chat");

    let first_root = MessageId::new();
    let second_root = MessageId::new();
    let first = TurnId::from(first_root);
    let second = TurnId::from(second_root);

    let (first_messages, first_state) = recorded(first, "c1", "plan", serde_json::json!(["a"]));
    let mut messages = vec![entry(
        first,
        Message::User(UserMessage::text(first_root, "start")),
    )];
    messages.extend(first_messages);
    let mut state = vec![first_state];
    storage
        .save_checkpoint(
            "chat".to_string(),
            StoredCheckpoint {
                state: state.clone(),
                ..checkpoint("chat", messages.clone())
            },
        )
        .await
        .unwrap();

    let (second_messages, second_state) =
        recorded(second, "c1", "plan", serde_json::json!(["a", "b"]));
    messages.push(entry(
        second,
        Message::User(UserMessage::text(second_root, "carry on")),
    ));
    messages.extend(second_messages);
    state.push(second_state);
    storage
        .save_checkpoint(
            "chat".to_string(),
            StoredCheckpoint {
                state,
                ..checkpoint("chat", messages)
            },
        )
        .await
        .unwrap();

    second_root
}

/// The value a thread holds now: its entries reduced last-wins, which is what
/// the runtime does on load.
async fn current_state(
    pool: &DbPool,
    workspace: &str,
    session: &str,
    thread: &str,
    kind: &str,
) -> Option<serde_json::Value> {
    PgSessionStorage::new(pool.clone(), workspace, session)
        .load_checkpoint(thread)
        .await
        .unwrap()
        .unwrap()
        .state
        .into_iter()
        .rfind(|entry| entry.kind == kind)
        .map(|entry| entry.value)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_takes_anchored_state_back_with_the_turns() {
    let pool = pool().await;
    let workspace = workspace_id("rewind-state");
    let cut = seed_session_with_state(&pool, &workspace).await;

    assert_eq!(
        current_state(&pool, &workspace, "chat", "chat", "plan").await,
        Some(serde_json::json!(["a", "b"])),
    );

    PgSessionStorage::new(pool.clone(), &workspace, "chat")
        .rewind_to(cut)
        .await
        .unwrap();

    assert_eq!(
        current_state(&pool, &workspace, "chat", "chat", "plan").await,
        Some(serde_json::json!(["a"])),
        "the second turn's value goes with the turn that recorded it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_needs_no_code_of_its_own_to_cut_state() {
    // The anchor is a foreign key onto `messages` with `on delete cascade`, so
    // deleting the messages is what removes the state. Rewinding to the opening
    // message must therefore leave no state row behind at all.
    let pool = pool().await;
    let workspace = workspace_id("rewind-state-all");
    seed_session_with_state(&pool, &workspace).await;
    let first_root = PgSessionStorage::new(pool.clone(), &workspace, "chat")
        .load_checkpoint("chat")
        .await
        .unwrap()
        .unwrap()
        .messages
        .into_iter()
        .next()
        .map(|entry| match &entry.message {
            Message::User(message) => message.message_id,
            other => panic!("expected the opening user message, got {other:?}"),
        })
        .unwrap();

    assert!(row_count(&pool, "thread_state", &workspace).await > 0);
    PgSessionStorage::new(pool.clone(), &workspace, "chat")
        .rewind_to(first_root)
        .await
        .unwrap();
    assert_eq!(row_count(&pool, "thread_state", &workspace).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_inherits_the_state_its_kept_turns_recorded() {
    let pool = pool().await;
    let workspace = workspace_id("fork-state");
    let cut = seed_session_with_state(&pool, &workspace).await;

    let forked = WorkspaceStorage::new(pool.clone(), &workspace)
        .fork_session("chat", ForkCut::At(cut), ForkSource::Cold)
        .await
        .unwrap();

    assert_eq!(
        current_state(
            &pool,
            &workspace,
            &forked.session_id,
            &forked.session_id,
            "plan"
        )
        .await,
        Some(serde_json::json!(["a"])),
        "the branch starts from what the kept turns recorded, not the source's latest"
    );
    assert_eq!(
        current_state(&pool, &workspace, "chat", "chat", "plan").await,
        Some(serde_json::json!(["a", "b"])),
        "and the source keeps its own"
    );
}
