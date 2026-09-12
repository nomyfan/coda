use super::*;

fn provider(id: &str, model: &str, family: Option<&str>, image: bool) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        kind: coda_openai::ProviderKind::Generic,
        api_key: "unused".into(),
        base_url: "http://127.0.0.1:1".into(),
        include_usage: true,
        models: vec![ModelConfig {
            id: model.into(),
            name: model.into(),
            family: family.map(str::to_owned),
            context_window: 100_000,
            max_completion_tokens: None,
            reasoning_efforts: vec!["low".into(), "high".into()],
            default_reasoning_effort: Some("high".into()),
            input_modalities: if image {
                vec![Modality::Text, Modality::Image]
            } else {
                vec![Modality::Text]
            },
            auto_compact_threshold: None,
        }],
    }
}

impl Harness {
    async fn restart(&mut self, configs: Vec<ProviderConfig>) {
        self.app.relay.shutdown_all().await;
        self.app.shutdown.cancel();
        self.streams.clear();
        self.selections.clear();
        let (providers, provider_catalog) = build_providers(configs);
        let shutdown = CancellationToken::new();
        self.workspace = Arc::new(
            build_workspace(
                WorkspaceConfig {
                    id: self.workspace.id.clone(),
                    path: self.dir.path().into(),
                },
                &providers,
                &self.pool,
                &shutdown,
            )
            .await
            .unwrap(),
        );
        let workspaces = HashMap::from([(self.workspace.id.clone(), self.workspace.clone())]);
        let relay = Arc::new(SessionHub::new(
            Arc::new(AppOpener {
                providers: providers.clone(),
                workspaces: workspaces.clone(),
                background_root: self.dir.path().join("background"),
            }),
            Default::default(),
        ));
        self.app = Arc::new(AppState {
            providers,
            default_provider: provider_catalog[0].id.clone(),
            provider_catalog,
            shutdown,
            workspaces,
            relay,
            keepalive: Default::default(),
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn removed_preview_recovers_manually_across_providers_and_retains_approvals() {
    let mut h = Harness::with_providers(vec![provider("p1", "preview", Some("f"), false)]).await;
    let binding = SessionModelBinding {
        provider_id: "p1".into(),
        model_id: "preview".into(),
        family: Some("f".into()),
        reasoning_effort: Some("low".into()),
    };
    h.workspace
        .storage
        .initialize_session("chat", binding.clone())
        .await
        .unwrap();
    h.workspace
        .storage
        .session("chat")
        .save_checkpoint("chat".into(), suspended("chat"))
        .await
        .unwrap();
    assert_eq!(
        h.request("open_session", json!({})).await["result"]["pending_approvals"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.restart(vec![
        provider("p2", "released", Some("f"), false),
        provider("other", "unrelated", None, false),
    ])
    .await;
    let before = h.execution_rows().await;
    let opened = h.request("open_session", json!({})).await;
    assert_eq!(
        opened["result"]["access"]["reason"], "model_not_configured",
        "{opened}"
    );
    assert_eq!(opened["result"]["provider_id"], "p1:preview");
    assert_eq!(opened["result"]["model_family"], "f");
    assert_eq!(opened["result"]["model_candidates"], json!(["p2:released"]));
    assert_eq!(
        h.workspace
            .storage
            .load_model_binding("chat")
            .await
            .unwrap(),
        binding
    );
    assert_eq!(h.execution_rows().await, before);
    let switched = h
        .request("set_model", json!({"provider_id": "p2:released"}))
        .await;
    assert_eq!(
        switched["result"]["provider_id"], "p2:released",
        "{switched}"
    );
    assert_eq!(switched["result"]["access"]["type"], "read_write");
    assert_eq!(switched["result"]["reasoning_effort"], "high");
    assert_eq!(
        switched["result"]["pending_approvals"],
        opened["result"]["pending_approvals"]
    );
    assert_eq!(switched["result"]["messages"], opened["result"]["messages"]);
    assert_eq!(h.execution_rows().await, before);
    assert!(
        h.request(
            "set_model",
            json!({"provider_id": "p2:released", "reasoning_effort": "low"})
        )
        .await
        .get("error")
        .is_some()
    );
    h.restart(vec![provider("p2", "released", Some("f"), false)])
        .await;
    let reopened = h.request("open_session", json!({})).await;
    assert_eq!(reopened["result"]["provider_id"], "p2:released");
    assert_eq!(
        reopened["result"]["pending_approvals"],
        switched["result"]["pending_approvals"]
    );
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fixed_family_rejects_same_model_effort_changes_noops_and_retries_after_config_drift() {
    for configured in [None, Some("g")] {
        let mut h = Harness::with_providers(vec![
            provider("p1", "m1", configured, false),
            provider("p2", "m1", Some("f"), false),
        ])
        .await;
        let binding = SessionModelBinding {
            provider_id: "p1".into(),
            model_id: "m1".into(),
            family: Some("f".into()),
            reasoning_effort: Some("low".into()),
        };
        h.workspace
            .storage
            .initialize_session("chat", binding.clone())
            .await
            .unwrap();
        let opened = h.request("open_session", json!({})).await;
        assert_eq!(opened["result"]["access"]["reason"], "model_family_changed");
        assert_eq!(opened["result"]["model_candidates"], json!(["p2:m1"]));
        for effort in ["low", "high", "low"] {
            let response = h
                .request(
                    "set_model",
                    json!({"provider_id": "p1:m1", "reasoning_effort": effort}),
                )
                .await;
            assert_eq!(
                response["error"]["code"],
                rpc::INVALID_MODEL_SELECTION,
                "{response}"
            );
            assert_eq!(
                h.workspace
                    .storage
                    .load_model_binding("chat")
                    .await
                    .unwrap(),
                binding
            );
        }
        let response = h
            .request(
                "set_model",
                json!({"provider_id": "p2:m1", "reasoning_effort": "low"}),
            )
            .await;
        assert_eq!(
            response["result"]["access"]["type"], "read_write",
            "{response}"
        );
        h.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_enrolls_unbound_history_and_persists_default_effort_before_switching() {
    let mut h = Harness::with_providers(vec![
        provider("p1", "m1", Some("f"), false),
        provider("p2", "m1", Some("f"), false),
    ])
    .await;
    let binding = SessionModelBinding {
        provider_id: "p1".into(),
        model_id: "m1".into(),
        family: None,
        reasoning_effort: None,
    };
    h.workspace
        .storage
        .initialize_session("chat", binding)
        .await
        .unwrap();
    let opened = h.request("open_session", json!({})).await;
    assert_eq!(opened["result"]["model_family"], "f");
    assert_eq!(opened["result"]["reasoning_effort"], "high");
    let saved = h
        .workspace
        .storage
        .load_model_binding("chat")
        .await
        .unwrap();
    assert_eq!(saved.family.as_deref(), Some("f"));
    assert_eq!(saved.reasoning_effort.as_deref(), Some("high"));
    let switched = h
        .request(
            "set_model",
            json!({"provider_id": "p2:m1", "reasoning_effort": "low"}),
        )
        .await;
    assert_eq!(switched["result"]["provider_id"], "p2:m1", "{switched}");
    h.restart(vec![provider("p2", "m1", Some("f"), false)])
        .await;
    let opened = h.request("open_session", json!({})).await;
    assert_eq!(opened["result"]["provider_id"], "p2:m1");
    assert_eq!(opened["result"]["reasoning_effort"], "low");
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn inherited_child_images_restrict_candidates_but_an_explicit_model_override_does_not() {
    let configs = || {
        vec![
            provider("p", "text", Some("f"), false),
            provider("p", "vision", Some("f"), true),
        ]
    };
    // Distinct provider configs need distinct ids; keep model keys stable across restart.
    let configs = || {
        let mut c = configs();
        c[1].id = "vision".into();
        c
    };
    let mut h = Harness::with_providers(configs()).await;
    h.workspace
        .storage
        .initialize_session(
            "chat",
            SessionModelBinding {
                provider_id: "removed".into(),
                model_id: "preview".into(),
                family: Some("f".into()),
                reasoning_effort: Some("low".into()),
            },
        )
        .await
        .unwrap();
    let mut child = suspended("worker");
    child.agent_name = "image-worker".into();
    child.parent_pid = Some("chat".into());
    child.derivation_key = Some("image-worker".into());
    child.resume_point = StoredResumePoint::Generation;
    let Message::User(user) = &mut child.messages[0].message else {
        unreachable!()
    };
    user.parts.push(coda_core::llm::ContentPart::Image {
        url: "data:image/png;base64,eA==".into(),
    });
    h.workspace
        .storage
        .session("chat")
        .save_checkpoint("worker".into(), child)
        .await
        .unwrap();
    let opened = h.request("open_session", json!({})).await;
    assert_eq!(
        opened["result"]["model_candidates"],
        json!(["vision:vision"])
    );
    let rejected = h
        .request("set_model", json!({"provider_id": "p:text"}))
        .await;
    assert_eq!(rejected["error"]["code"], rpc::INVALID_MODEL_SELECTION);
    assert!(rejected["error"]["data"].to_string().contains("image"));
    let agent_dir = h.dir.path().join(".coda/agents/image-worker");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("AGENT.md"),
        "---\ndescription: image worker\nmode: stateful\nmodel: vision:vision\n---\nHandle images.",
    )
    .unwrap();
    h.restart(configs()).await;
    let opened = h.request("open_session", json!({})).await;
    assert_eq!(
        opened["result"]["model_candidates"],
        json!(["p:text", "vision:vision"]),
        "{opened}"
    );
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_cold_scope_cleanup_keeps_the_committed_model_and_can_retry() {
    let mut h = Harness::with_providers(vec![provider("p", "released", Some("f"), false)]).await;
    h.workspace
        .storage
        .initialize_session(
            "chat",
            SessionModelBinding {
                provider_id: "p".into(),
                model_id: "removed-preview".into(),
                family: Some("f".into()),
                reasoning_effort: Some("low".into()),
            },
        )
        .await
        .unwrap();
    h.workspace
        .storage
        .session("chat")
        .save_checkpoint("chat".into(), suspended("chat"))
        .await
        .unwrap();
    let mut child = suspended("worker");
    child.parent_pid = Some("chat".into());
    child.derivation_key = Some("worker".into());
    let task_id = TaskId::new();
    child.active_execution = Some(StoredExecution {
        invocation_id: "interrupted".into(),
        scope: ProcessGroupId::Background {
            task_id: task_id.clone(),
        },
        completion: CompletionTarget::BackgroundTask(task_id),
        agent_path: vec!["coda".into(), "coda".into()],
    });
    h.workspace
        .storage
        .session("chat")
        .save_checkpoint("worker".into(), child)
        .await
        .unwrap();
    let opened = h.request("open_session", json!({})).await;
    assert_eq!(opened["result"]["access"]["reason"], "model_not_configured");
    let before = h.execution_rows().await;
    let constraint = format!("reject_cleanup_{}", uuid::Uuid::new_v4().simple());
    let mut conn = h.pool.get().await.unwrap();
    diesel::sql_query(format!("ALTER TABLE aborted_executions ADD CONSTRAINT {constraint} CHECK (workspace_id <> '{}') NOT VALID", h.workspace.id)).execute(&mut conn).await.unwrap();
    let failed = h
        .request("set_model", json!({"provider_id": "p:released"}))
        .await;
    diesel::sql_query(format!(
        "ALTER TABLE aborted_executions DROP CONSTRAINT {constraint}"
    ))
    .execute(&mut conn)
    .await
    .unwrap();
    assert_eq!(
        failed["result"]["access"]["reason"], "runtime_open_failed",
        "{failed}"
    );
    assert_eq!(failed["result"]["provider_id"], "p:released");
    assert_eq!(
        failed["result"]["pending_approvals"],
        opened["result"]["pending_approvals"]
    );
    assert_eq!(h.execution_rows().await, before);
    assert_eq!(
        h.workspace
            .storage
            .load_model_binding("chat")
            .await
            .unwrap()
            .model_id,
        "released"
    );
    let recovered = h
        .request("set_model", json!({"provider_id": "p:released"}))
        .await;
    assert_eq!(
        recovered["result"]["access"]["type"], "read_write",
        "{recovered}"
    );
    assert_eq!(
        recovered["result"]["pending_approvals"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(recovered["result"]["pending_approvals"][0]["pid"], "chat");
    h.finish().await;
}
