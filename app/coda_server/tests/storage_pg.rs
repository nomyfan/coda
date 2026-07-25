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

use sqlx::{PgPool, Row};

/// A fresh pool per test. A sqlx pool is tied to the runtime that created it and
/// `#[tokio::test]` gives every test its own, so a pool shared through a static
/// starts timing out the moment the first test's runtime shuts down. Connections
/// are opened on demand, and the migrator takes a PostgreSQL advisory lock, so
/// paying for a pool per test costs one connection and one version check.
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must point at a throwaway PostgreSQL database \
         (this suite migrates, writes and deletes)",
    );
    coda_server::storage::connect(&url)
        .await
        .expect("connect to DATABASE_URL and apply migrations")
}

fn workspace_id(test: &str) -> String {
    format!("{test}-{}", uuid::Uuid::new_v4())
}

#[tokio::test]
async fn deleting_a_session_takes_its_threads_messages_and_snapshot_with_it() {
    let pool = pool().await;
    let workspace = workspace_id("cascade");

    sqlx::query(
        "insert into sessions (workspace_id, session_id, model_binding)
         values ($1, 'doomed', '{}'::jsonb), ($1, 'keeper', '{}'::jsonb)",
    )
    .bind(&workspace)
    .execute(&pool)
    .await
    .unwrap();

    for session in ["doomed", "keeper"] {
        sqlx::query(
            "insert into thread_checkpoints
                (workspace_id, session_id, thread_id, agent_name, resume_point,
                 todos, suspended_at, message_count, pending_approval)
             values ($1, $2, $2, 'coda', '\"Generation\"'::jsonb, '[]'::jsonb, now(), 1, false)",
        )
        .bind(&workspace)
        .bind(session)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "insert into messages
                (workspace_id, session_id, thread_id, seq, message_id, turn_id, role, payload)
             values ($1, $2, $2, 0, gen_random_uuid(), gen_random_uuid(), 'user', '{}'::jsonb)",
        )
        .bind(&workspace)
        .bind(session)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "insert into runtime_snapshots (workspace_id, session_id, snapshot)
             values ($1, $2, '{}'::jsonb)",
        )
        .bind(&workspace)
        .bind(session)
        .execute(&pool)
        .await
        .unwrap();
    }

    sqlx::query("delete from sessions where workspace_id = $1 and session_id = 'doomed'")
        .bind(&workspace)
        .execute(&pool)
        .await
        .unwrap();

    // Everything owned by the deleted session is gone, and the sibling session
    // is untouched — the cascade follows the composite key, not just the id.
    for table in ["thread_checkpoints", "messages", "runtime_snapshots"] {
        let surviving: Vec<String> = sqlx::query(&format!(
            "select session_id from {table} where workspace_id = $1"
        ))
        .bind(&workspace)
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
        assert_eq!(surviving, vec!["keeper"], "{table} was not cascaded");
    }
}

#[tokio::test]
async fn a_thread_cannot_belong_to_a_session_that_does_not_exist() {
    let pool = pool().await;
    let workspace = workspace_id("orphan");

    let orphan = sqlx::query(
        "insert into thread_checkpoints
            (workspace_id, session_id, thread_id, agent_name, resume_point,
             todos, suspended_at, message_count, pending_approval)
         values ($1, 'never-opened', 'never-opened', 'coda', '\"Generation\"'::jsonb,
                 '[]'::jsonb, now(), 0, false)",
    )
    .bind(&workspace)
    .execute(&pool)
    .await;

    assert!(
        orphan.is_err(),
        "the foreign key must reject a checkpoint with no session row"
    );
}
