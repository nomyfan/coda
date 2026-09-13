use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn model_histories_batch_processes_and_exclude_overrides_before_reading_payloads() {
    use diesel::connection::InstrumentationEvent;
    use diesel_async::AsyncConnection;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let pool = pool().await;
    pool.resize(1);
    let workspace = workspace_id("model-histories");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    storage
        .initialize_session("s", test_binding())
        .await
        .unwrap();
    let mut expected = std::collections::HashMap::new();
    for index in 0..12 {
        let pid = format!("worker-{index}");
        let turn = TurnId::from(MessageId::new());
        let mut image = UserMessage::text(MessageId::new(), "old image");
        image.parts.push(coda_core::llm::ContentPart::Image {
            url: "data:image/png;base64,eA==".into(),
        });
        let reply = assistant("image description");
        let cutoff = reply.message_id();
        let history = vec![
            entry(turn, Message::User(image)),
            entry(turn, reply),
            entry(turn, summary_message(cutoff, "compacted image")),
            entry(
                TurnId::from(MessageId::new()),
                Message::User(UserMessage::text(MessageId::new(), format!("tail-{index}"))),
            ),
        ];
        storage
            .session("s")
            .save_checkpoint(pid.clone(), checkpoint(&pid, history.clone()))
            .await
            .unwrap();
        expected.insert(pid, history);
    }
    storage
        .session("s")
        .save_checkpoint("empty".into(), checkpoint("empty", vec![]))
        .await
        .unwrap();
    let mut overridden = checkpoint(
        "override",
        vec![entry(TurnId::from(MessageId::new()), assistant("unused"))],
    );
    overridden.agent_name = "own-model".into();
    storage
        .session("s")
        .save_checkpoint("override".into(), overridden)
        .await
        .unwrap();
    // Same PID in another session and workspace must never join into this history.
    for (scope, session) in [
        (storage.clone(), "other"),
        (
            WorkspaceStorage::new(pool.clone(), workspace_id("other-model-histories")),
            "s",
        ),
    ] {
        scope
            .initialize_session(session, test_binding())
            .await
            .unwrap();
        scope
            .session(session)
            .save_checkpoint(
                "worker-0".into(),
                checkpoint(
                    "worker-0",
                    vec![entry(
                        TurnId::from(MessageId::new()),
                        assistant("unrelated"),
                    )],
                ),
            )
            .await
            .unwrap();
    }
    let mut connection = conn(&pool).await;
    diesel::sql_query("UPDATE messages SET payload = '{}'::jsonb WHERE workspace_id = $1 AND session_id = 's' AND pid = 'override'")
        .bind::<Text, _>(&workspace).execute(&mut connection).await.unwrap();
    let queries = Arc::new(AtomicUsize::new(0));
    let observed = queries.clone();
    connection.set_instrumentation(move |event: InstrumentationEvent<'_>| {
        if let InstrumentationEvent::StartQuery { query, .. } = event {
            // Exclude pool health checks; count actual checkpoint/history reads.
            let sql = query.to_string();
            if sql.contains("\"messages\"") || sql.contains("\"process_checkpoints\"") {
                observed.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    drop(connection);

    let histories = storage.model_histories("s", &["own-model"]).await.unwrap();
    assert_eq!(
        queries.as_ref().load(Ordering::SeqCst),
        1,
        "history reads must not grow with the process count"
    );
    assert_eq!(
        serde_json::to_value(&histories).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    for history in histories.values() {
        let visible: Vec<_> = coda_agent::message_view::model_view(history).collect();
        assert_eq!(visible.len(), 2);
        assert!(
            !visible
                .iter()
                .any(|entry| matches!(&entry.message, Message::User(user) if user.has_image()))
        );
    }
    // The malformed payload must still fail if its agent inherits the session model.
    assert!(storage.model_histories("s", &[]).await.is_err());
}

#[derive(QueryableByName)]
struct BackendPid {
    #[diesel(sql_type = Integer)]
    pid: i32,
}

#[tokio::test(flavor = "multi_thread")]
async fn confirmation_waits_for_the_writer_before_accepting_an_old_binding() {
    let pool = pool().await;
    for commit in [true, false] {
        let workspace = workspace_id("family-confirmation");
        let storage = WorkspaceStorage::new(pool.clone(), &workspace);
        let old = test_binding();
        let next = SessionModelBinding {
            provider_id: "replacement".into(),
            family: Some("f".into()),
            ..old.clone()
        };
        storage.initialize_session("s", old.clone()).await.unwrap();
        let mut writer = conn(&pool).await;
        let writer_pid = diesel::sql_query("SELECT pg_backend_pid() AS pid")
            .get_result::<BackendPid>(&mut writer)
            .await
            .unwrap()
            .pid;
        diesel::sql_query("BEGIN")
            .execute(&mut writer)
            .await
            .unwrap();
        diesel::sql_query("UPDATE sessions SET model_binding = $1::jsonb WHERE workspace_id = $2 AND session_id = 's'")
            .bind::<Text, _>(serde_json::to_string(&next).unwrap())
            .bind::<Text, _>(&workspace).execute(&mut writer).await.unwrap();

        // This is the misleading observation from the original design.
        assert_eq!(storage.load_model_binding("s").await.unwrap(), old);
        let confirming = storage.clone();
        let read = tokio::spawn(async move { confirming.confirm_model_binding("s").await });
        let mut observer = conn(&pool).await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let blocked = diesel::sql_query("SELECT EXISTS (SELECT FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))) AS ok")
                    .bind::<Integer, _>(writer_pid).get_result::<BoolRow>(&mut observer).await.unwrap().ok;
                if blocked { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("confirmation must wait on the writer's lock");
        assert!(!read.is_finished());
        diesel::sql_query(if commit { "COMMIT" } else { "ROLLBACK" })
            .execute(&mut writer)
            .await
            .unwrap();
        assert_eq!(
            read.await.unwrap().unwrap(),
            if commit { next } else { old }
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_an_existing_session_waits_for_a_previous_binding_write() {
    let pool = pool().await;
    let workspace = workspace_id("family-attach");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    let old = test_binding();
    let next = SessionModelBinding {
        family: Some("f".into()),
        ..old.clone()
    };
    storage.initialize_session("s", old.clone()).await.unwrap();
    let mut writer = conn(&pool).await;
    let pid = diesel::sql_query("SELECT pg_backend_pid() AS pid")
        .get_result::<BackendPid>(&mut writer)
        .await
        .unwrap()
        .pid;
    diesel::sql_query("BEGIN")
        .execute(&mut writer)
        .await
        .unwrap();
    diesel::sql_query("UPDATE sessions SET model_binding = $1::jsonb WHERE workspace_id = $2 AND session_id = 's'")
        .bind::<Text, _>(serde_json::to_string(&next).unwrap()).bind::<Text, _>(&workspace)
        .execute(&mut writer).await.unwrap();
    let opening = storage.clone();
    let attach = tokio::spawn(async move { opening.initialize_session("s", old).await });
    let mut observer = conn(&pool).await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if diesel::sql_query("SELECT EXISTS (SELECT FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))) AS ok")
                .bind::<Integer, _>(pid).get_result::<BoolRow>(&mut observer).await.unwrap().ok { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert!(!attach.is_finished());
    diesel::sql_query("COMMIT")
        .execute(&mut writer)
        .await
        .unwrap();
    assert_eq!(attach.await.unwrap().unwrap(), next);
}

#[tokio::test(flavor = "multi_thread")]
async fn binding_compare_exchange_accepts_missing_family_and_checks_effort_too() {
    let pool = pool().await;
    let workspace = workspace_id("family-cas");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    let old = test_binding();
    storage.initialize_session("s", old.clone()).await.unwrap();
    diesel::sql_query(
        "UPDATE sessions SET model_binding = model_binding - 'family' WHERE workspace_id = $1",
    )
    .bind::<Text, _>(&workspace)
    .execute(&mut conn(&pool).await)
    .await
    .unwrap();
    let next = SessionModelBinding {
        family: Some("f".into()),
        ..old.clone()
    };
    assert_eq!(
        storage
            .compare_exchange_model_binding("s", &old, &next)
            .await
            .unwrap(),
        next
    );
    let changed = SessionModelBinding {
        reasoning_effort: Some("low".into()),
        ..next.clone()
    };
    storage
        .compare_exchange_model_binding("s", &next, &changed)
        .await
        .unwrap();
    assert_eq!(
        storage
            .compare_exchange_model_binding("s", &next, &old)
            .await,
        Err(SessionMetadataError::BindingMismatch)
    );
    assert_eq!(storage.confirm_model_binding("s").await.unwrap(), changed);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_confirmation_timeout_does_not_prove_the_write_was_rolled_back() {
    let pool = pool().await;
    let workspace = workspace_id("family-timeout");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    let old = test_binding();
    let next = SessionModelBinding {
        family: Some("f".into()),
        ..old.clone()
    };
    storage.initialize_session("s", old.clone()).await.unwrap();
    let mut writer = conn(&pool).await;
    diesel::sql_query("BEGIN")
        .execute(&mut writer)
        .await
        .unwrap();
    diesel::sql_query("UPDATE sessions SET model_binding = $1::jsonb WHERE workspace_id = $2 AND session_id = 's'")
        .bind::<Text, _>(serde_json::to_string(&next).unwrap()).bind::<Text, _>(&workspace)
        .execute(&mut writer).await.unwrap();
    let error = storage.confirm_model_binding("s").await.unwrap_err();
    assert!(error.to_string().contains("lock timeout"), "{error}");
    assert_eq!(storage.load_model_binding("s").await.unwrap(), old);
    diesel::sql_query("COMMIT")
        .execute(&mut writer)
        .await
        .unwrap();
    assert_eq!(storage.confirm_model_binding("s").await.unwrap(), next);
}

#[tokio::test(flavor = "multi_thread")]
async fn fork_and_rewind_preserve_historical_generation_and_the_current_family_binding() {
    let pool = pool().await;
    let workspace = workspace_id("family-provenance");
    let storage = WorkspaceStorage::new(pool.clone(), &workspace);
    let binding = SessionModelBinding {
        family: Some("f".into()),
        provider_id: "p2".into(),
        model_id: "released".into(),
        reasoning_effort: Some("low".into()),
    };
    storage
        .initialize_session("s", binding.clone())
        .await
        .unwrap();
    let first = MessageId::new();
    let cut = MessageId::new();
    let generation = coda_core::llm::GenerationMetadata {
        provider_id: "removed".into(),
        model_id: "preview".into(),
        reasoning_effort: Some("high".into()),
    };
    let Message::Assistant(mut recorded) = assistant("from the preview model") else {
        unreachable!()
    };
    recorded.generation = Some(generation.clone());
    let messages = vec![
        entry(
            first.into(),
            Message::User(UserMessage::text(first, "first question")),
        ),
        entry(first.into(), Message::Assistant(recorded)),
        entry(
            cut.into(),
            Message::User(UserMessage::text(cut, "later question")),
        ),
        entry(cut.into(), assistant("legacy reply with no provenance")),
    ];
    storage
        .session("s")
        .save_checkpoint("s".into(), checkpoint("s", messages))
        .await
        .unwrap();
    let fork = storage
        .fork_session("s", ForkCut::All, ForkSource::Cold)
        .await
        .unwrap();
    assert_eq!(
        storage.load_model_binding(&fork.session_id).await.unwrap(),
        binding
    );
    let copied = storage
        .session(&fork.session_id)
        .load_checkpoint(&fork.session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&copied.messages[1].message, Message::Assistant(a) if a.generation.as_ref() == Some(&generation))
    );
    assert!(matches!(&copied.messages[3].message, Message::Assistant(a) if a.generation.is_none()));
    let retained = storage.session("s").rewind_to(cut).await.unwrap();
    assert_eq!(retained.len(), 2);
    assert!(
        matches!(&retained[1], Message::Assistant(a) if a.generation.as_ref() == Some(&generation))
    );
    assert_eq!(storage.load_model_binding("s").await.unwrap(), binding);
}
