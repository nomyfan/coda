use super::*;
use diesel::sql_types::Jsonb;
use diesel_async::{AsyncConnection, SimpleAsyncConnection};
use serde_json::{Value, json};

#[derive(QueryableByName)]
struct MigrationJson {
    #[diesel(sql_type = Jsonb)]
    value: Value,
}

#[tokio::test(flavor = "multi_thread")]
async fn process_migration_converts_runtime_metadata_without_rewriting_content() {
    let pool = pool().await;
    let mut db = conn(&pool).await;
    db.begin_test_transaction().await.unwrap();
    // Shadow the real tables so the actual migration SQL can run in isolation.
    db.batch_execute(
        "CREATE TEMP TABLE sessions (
            workspace_id text, session_id text, PRIMARY KEY (workspace_id, session_id)
        ) ON COMMIT DROP;
        CREATE TEMP TABLE thread_checkpoints (
            thread_id text PRIMARY KEY, parent_thread_id text, active_execution jsonb,
            workspace_id text, session_id text,
            FOREIGN KEY (workspace_id, session_id) REFERENCES sessions(workspace_id, session_id)
        ) ON COMMIT DROP;
        CREATE TEMP TABLE messages (thread_id text, payload jsonb) ON COMMIT DROP;
        CREATE TEMP TABLE aborted_executions (thread_id text) ON COMMIT DROP;
        CREATE TEMP TABLE runtime_snapshots (
            workspace_id text, session_id text, snapshot jsonb,
            PRIMARY KEY (workspace_id, session_id)
        ) ON COMMIT DROP;",
    )
    .await
    .unwrap();

    let content = r#"{"thread_id":"user data","active_threads":{"keep":"this"}}"#;
    let old_task = json!({
        "id": "task", "from": "User", "to": {"name": "worker", "thread_id": "child"},
        "reply_to": null,
        "body": {"Task": {"message_id": MessageId::new(), "task": content}}
    });
    let old_reply = json!({
        "id": "reply", "from": {"Agent": {"name": "worker", "thread_id": "child"}},
        "to": {"name": "root", "thread_id": "root"}, "reply_to": "call",
        "body": {"Reply": {"call_id": "call", "output": {"Ok": content}, "aborted": false}}
    });
    let new_task = json!({
        "id": "task", "from": "User", "to": {"name": "worker", "pid": "child"},
        "reply_to": null, "body": old_task["body"]
    });
    let new_reply = json!({
        "id": "reply", "from": {"Agent": {"name": "worker", "pid": "child"}},
        "to": {"name": "root", "pid": "root"}, "reply_to": "call", "body": old_reply["body"]
    });
    // Map keys are opaque process IDs, even when an ID happens to be "thread_id".
    let old_snapshot = json!({
        "active_threads": {"thread_id": "worker"},
        "drained_envelopes": {"thread_id": [old_task, old_reply]},
        "agent_drained_envelopes": {"root": [old_reply], "empty": []}
    });
    let new_snapshot = json!({
        "active_processes": {"thread_id": "worker"},
        "drained_envelopes": {"thread_id": [new_task, new_reply]},
        "agent_drained_envelopes": {"root": [new_reply], "empty": []}
    });
    let old_idle = json!({
        "active_threads": {}, "drained_envelopes": {}, "agent_drained_envelopes": {}
    });
    let new_idle = json!({
        "active_processes": {}, "drained_envelopes": {}, "agent_drained_envelopes": {}
    });
    for (session, snapshot) in [
        ("legacy", &old_snapshot),
        ("current", &new_snapshot),
        ("idle", &old_idle),
    ] {
        diesel::sql_query("INSERT INTO runtime_snapshots VALUES ('test', $1, $2)")
            .bind::<Text, _>(session)
            .bind::<Jsonb, _>(snapshot)
            .execute(&mut *db)
            .await
            .unwrap();
    }
    let old_execution = json!({
        "invocation_id": "call", "scope": {"Foreground": {"turn_id": TurnId::from(MessageId::new())}},
        "agent_path": ["root", "worker"],
        "completion": {"Caller": {
            "envelope_id": "call", "sender_name": "root",
            "sender_thread_id": "root", "call_id": "tool"
        }}
    });
    let new_execution = json!({
        "invocation_id": "call", "scope": old_execution["scope"],
        "agent_path": ["root", "worker"],
        "completion": {"Caller": {
            "envelope_id": "call", "sender_name": "root",
            "sender_pid": "root", "call_id": "tool"
        }}
    });
    for (pid, execution) in [("legacy", &old_execution), ("current", &new_execution)] {
        diesel::sql_query(
            "INSERT INTO thread_checkpoints (thread_id, active_execution) VALUES ($1, $2)",
        )
        .bind::<Text, _>(pid)
        .bind::<Jsonb, _>(execution)
        .execute(&mut *db)
        .await
        .unwrap();
    }
    db.batch_execute("INSERT INTO thread_checkpoints (thread_id) VALUES ('idle')")
        .await
        .unwrap();

    let up = include_str!("../../migrations/20260906000000_process_identity/up.sql");
    let down = include_str!("../../migrations/20260906000000_process_identity/down.sql");
    for sql in [up, down, up] {
        db.batch_execute(sql).await.unwrap();
        let upgrading = sql == up;
        let expected_snapshot = if upgrading {
            &new_snapshot
        } else {
            &old_snapshot
        };
        let expected_idle = if upgrading { &new_idle } else { &old_idle };
        let snapshots: MigrationJson = diesel::sql_query(
            "SELECT jsonb_object_agg(session_id, snapshot) AS value FROM runtime_snapshots",
        )
        .get_result(&mut *db)
        .await
        .unwrap();
        assert_eq!(
            snapshots.value,
            json!({
                "legacy": expected_snapshot, "current": expected_snapshot, "idle": expected_idle
            })
        );
        let query = if upgrading {
            "SELECT jsonb_object_agg(pid, active_execution) AS value FROM process_checkpoints"
        } else {
            "SELECT jsonb_object_agg(thread_id, active_execution) AS value FROM thread_checkpoints"
        };
        let executions: MigrationJson =
            diesel::sql_query(query).get_result(&mut *db).await.unwrap();
        let expected_execution = if upgrading {
            &new_execution
        } else {
            &old_execution
        };
        assert_eq!(
            executions.value,
            json!({
                "legacy": expected_execution, "current": expected_execution, "idle": null
            })
        );
        if upgrading {
            // These are the types that previously failed when opening a session.
            serde_json::from_value::<StoredRuntimeSnapshot>(snapshots.value["legacy"].clone())
                .unwrap();
            serde_json::from_value::<StoredRuntimeSnapshot>(snapshots.value["idle"].clone())
                .unwrap();
            serde_json::from_value::<coda_agent::execution::StoredExecution>(
                executions.value["legacy"].clone(),
            )
            .unwrap();
        }
    }
}
