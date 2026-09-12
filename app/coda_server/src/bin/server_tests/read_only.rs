//! Real dispatcher and PostgreSQL coverage; DATABASE_URL must name a throwaway database.
use super::*;
use coda_agent::execution::{CompletionTarget, ProcessGroupId, StoredExecution};
use coda_agent::persist::{StoredCheckpoint, StoredPreparedToolCall, StoredResumePoint};
use coda_core::llm::{AssistantMessage, MessageId, ToolCall, UserMessage};
use coda_execution::{TaskExit, TaskId, TaskMeta};
use coda_server::config::ModelConfig;
use diesel::{QueryableByName, sql_types::Text};
use diesel_async::RunQueryDsl;
use serde_json::json;

struct NoReplyTransport;
impl Transport for NoReplyTransport {
    async fn recv(&self) -> Option<String> {
        None
    }
    async fn send(&self, _: &RpcOutgoing) -> bool {
        panic!("notifications must not reply")
    }
}

struct Harness {
    app: Arc<AppState>,
    workspace: Arc<WorkspaceState>,
    streams: StreamMap<SessionKey, BoxStream<'static, RelayEvent>>,
    selections: HashMap<SessionKey, Selection>,
    pool: DbPool,
    dir: tempfile::TempDir,
}

impl Harness {
    async fn new() -> Self {
        Self::with_providers(vec![ProviderConfig {
            id: "test".into(),
            kind: coda_openai::ProviderKind::Generic,
            api_key: "unused".into(),
            base_url: "http://127.0.0.1:1".into(),
            include_usage: true,
            models: vec![ModelConfig {
                family: None,
                id: "available".into(),
                name: "Available".into(),
                context_window: 100_000,
                max_completion_tokens: None,
                reasoning_efforts: vec!["low".into()],
                default_reasoning_effort: None,
                input_modalities: vec![Modality::Text],
                auto_compact_threshold: None,
            }],
        }])
        .await
    }

    async fn with_providers(configs: Vec<ProviderConfig>) -> Self {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must name a throwaway database");
        let pool = coda_server::storage::connect(&url).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (providers, provider_catalog) = build_providers(configs);
        let shutdown = CancellationToken::new();
        let workspace = Arc::new(
            build_workspace(
                WorkspaceConfig {
                    id: format!("readonly-{}", uuid::Uuid::new_v4()),
                    path: dir.path().into(),
                },
                &providers,
                &pool,
                &shutdown,
            )
            .await
            .unwrap(),
        );
        let workspaces = HashMap::from([(workspace.id.clone(), workspace.clone())]);
        let relay = Arc::new(SessionHub::new(
            Arc::new(AppOpener {
                providers: providers.clone(),
                workspaces: workspaces.clone(),
                background_root: dir.path().join("background"),
            }),
            Default::default(),
        ));
        let app = Arc::new(AppState {
            providers,
            default_provider: provider_catalog[0].id.clone(),
            provider_catalog,
            shutdown,
            workspaces,
            relay,
            keepalive: Default::default(),
        });
        Self {
            app,
            workspace,
            streams: StreamMap::new(),
            selections: HashMap::new(),
            pool,
            dir,
        }
    }

    async fn request(&mut self, method: &str, fields: Value) -> Value {
        let mut params = json!({"workspace_id": self.workspace.id, "session_id": "chat"});
        params
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let result = if answers_off_loop(method) {
            dispatch_off_loop(&self.app, 1, json!(1), method, params).await
        } else {
            dispatch_request(
                &self.app,
                1,
                &mut self.streams,
                &mut self.selections,
                json!(1),
                method,
                params,
            )
            .await
        };
        serde_json::to_value(result).unwrap()
    }

    /// Include row versions so rewriting the same checkpoint still fails the assertion.
    async fn execution_rows(&self) -> Vec<String> {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = Text)]
            payload: String,
        }
        let mut conn = self.pool.get().await.unwrap();
        let mut rows = Vec::new();
        for table in [
            "process_checkpoints",
            "messages",
            "runtime_snapshots",
            "task_notice_receipts",
            "aborted_executions",
        ] {
            let sql = format!(
                "SELECT COALESCE(jsonb_agg(to_jsonb(t) || jsonb_build_object('version', xmin::text) ORDER BY to_jsonb(t)::text), '[]'::jsonb)::text AS payload FROM {table} t WHERE workspace_id = $1"
            );
            rows.push(
                diesel::sql_query(sql)
                    .bind::<Text, _>(&self.workspace.id)
                    .get_result::<Row>(&mut conn)
                    .await
                    .unwrap()
                    .payload,
            );
        }
        rows
    }

    async fn finish(self) {
        self.app.relay.shutdown_all().await;
        self.app.shutdown.cancel();
        self.workspace.storage.delete_session("chat").await.unwrap();
    }
}

fn suspended(pid: &str) -> StoredCheckpoint {
    let message_id = MessageId::new();
    let call = ToolCall {
        id: format!("call-{pid}"),
        name: "shell".into(),
        arguments: Some(r#"{"command":"echo saved"}"#.into()),
    };
    let assistant = AssistantMessage {
        generation: None,
        message_id,
        content: "saved reply".into(),
        tool_calls: vec![call.clone()],
        usage: None,
        reasoning_content: None,
        reasoning_continuation: None,
        reasoning_ended_at: None,
        aborted: false,
        started_at: jiff::Timestamp::default(),
        ended_at: jiff::Timestamp::default(),
    };
    let user_id = MessageId::new();
    let turn = TurnId::from(user_id);
    StoredCheckpoint {
        pid: pid.into(),
        agent_name: "coda".into(),
        parent_pid: None,
        derivation_key: None,
        active_execution: None,
        messages: vec![
            HistoryEntry::new(
                turn,
                Message::User(UserMessage::text(user_id, "saved prompt")),
            ),
            HistoryEntry::new(turn, Message::Assistant(assistant)),
        ],
        resume_point: StoredResumePoint::PendingApproval {
            parent_message_id: message_id,
            pending_approval_calls: vec![StoredPreparedToolCall {
                tool_call: call,
                metadata: None,
            }],
            pending_calls: vec![],
        },
        suspended_at: jiff::Timestamp::default(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_rpc_preserves_removed_agent_approvals_and_all_execution_rows() {
    let mut h = Harness::new().await;
    let binding = SessionModelBinding {
        family: None,
        provider_id: "removed".into(),
        model_id: "model".into(),
        reasoning_effort: Some("old-effort".into()),
    };
    h.workspace
        .storage
        .initialize_session("chat", binding.clone())
        .await
        .unwrap();
    let storage = h.workspace.storage.session("chat");
    let root = suspended("chat");
    storage
        .save_checkpoint("chat".into(), root.clone())
        .await
        .unwrap();
    let mut child = suspended("worker");
    child.agent_name = "removed-agent".into();
    child.parent_pid = Some("chat".into());
    child.derivation_key = Some("removed-agent".into());
    let task_id = TaskId::new();
    child.active_execution = Some(StoredExecution {
        invocation_id: "saved-invocation".into(),
        scope: ProcessGroupId::Background {
            task_id: task_id.clone(),
        },
        completion: CompletionTarget::BackgroundTask(task_id.clone()),
        agent_path: vec!["coda".into(), "removed-agent".into()],
    });
    storage
        .save_checkpoint("worker".into(), child)
        .await
        .unwrap();
    let before = h.execution_rows().await;
    let expected = json!({"type": "read_only", "reason": "model_not_configured"});
    let cold_fork = h.request("fork_session", json!({})).await;
    assert_eq!(cold_fork["error"]["code"], rpc::SESSION_READ_ONLY);
    let opened = h
        .request("open_session", json!({"provider_id": "test:available"}))
        .await;
    assert_eq!(opened["result"]["access"], expected, "{opened}");
    assert_eq!(opened["result"]["provider_id"], "removed:model");
    assert_eq!(opened["result"]["reasoning_effort"], "old-effort");
    assert_eq!(opened["result"]["messages"].as_array().unwrap().len(), 2);
    let approvals = opened["result"]["pending_approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 2);
    let worker = approvals.iter().find(|p| p["pid"] == "worker").unwrap();
    assert_eq!(worker["agent_path"], json!(["coda", "removed-agent"]));
    assert_eq!(worker["task_id"], task_id.to_string());
    assert_eq!(
        h.request("list_workspaces", json!({})).await["result"]["workspaces"][0]["sessions"][0]["access"],
        expected
    );
    let StoredResumePoint::PendingApproval {
        parent_message_id, ..
    } = root.resume_point
    else {
        unreachable!()
    };
    for (method, params) in [
        (
            "task",
            json!({"task": "continue", "images": ["https://example.com/image.png"]}),
        ),
        (
            "rewind",
            json!({"message_id": MessageId::new(), "task": "rewrite", "images": ["https://example.com/image.png"]}),
        ),
        (
            "resume",
            json!({"agent_name": "coda", "pid": "chat", "decision": {"parent_message_id": parent_message_id, "resolutions": [["call-chat", "Execute"]]}, "allow_patterns": [["call-chat", "echo *"]]}),
        ),
        ("compact", json!({"instructions": "summary"})),
        ("set_permission_mode", json!({"mode": "yolo"})),
        ("fork_session", json!({})),
    ] {
        let reply = h.request(method, params).await;
        assert_eq!(
            reply["error"]["code"],
            rpc::SESSION_READ_ONLY,
            "{method}: {reply}"
        );
        assert_eq!(reply["error"]["data"]["reason"], "model_not_configured");
    }
    assert_eq!(
        h.request(
            "set_model",
            json!({"provider_id": "test:available", "reasoning_effort": "low"})
        )
        .await["error"]["code"],
        rpc::INVALID_MODEL_SELECTION
    );
    assert_eq!(
        h.request("add_allow_pattern", json!({"pattern": "echo *"}))
            .await["error"]["code"],
        rpc::METHOD_NOT_FOUND
    );
    for method in ["abort", "kill_task"] {
        assert!(
            dispatch_notification(
                &NoReplyTransport,
                &h.app,
                1,
                &mut h.streams,
                &mut h.selections,
                method,
                json!({"workspace_id": h.workspace.id, "session_id": "chat", "task_id": task_id})
            )
            .await
        );
    }
    assert!(!h.dir.path().join(".coda/config.toml").exists());
    assert!(!h.dir.path().join("background").exists());
    assert_eq!(before, h.execution_rows().await);
    assert_eq!(
        h.workspace
            .storage
            .load_model_binding("chat")
            .await
            .unwrap(),
        binding
    );
    // The internal reattach path receives the unavailable key cached in Selection.
    let key = (h.workspace.id.clone(), "chat".into());
    h.app.relay.detach(key.clone(), 1).await;
    let reopened = attach_core(
        &h.app,
        1,
        &mut h.streams,
        &mut h.selections,
        key,
        Some("removed:model".into()),
        Some("old-effort".into()),
        PermissionMode::Explore,
        false,
    )
    .await
    .unwrap();
    assert_eq!(serde_json::to_value(reopened.access).unwrap(), expected);
    assert_eq!(before, h.execution_rows().await);
    assert_eq!(
        h.request("rename_session", json!({"name": "Readable"}))
            .await["result"]["name"],
        "Readable"
    );
    assert!(
        h.request("delete_session", json!({}))
            .await
            .get("result")
            .is_some()
    );
    assert!(storage.load_checkpoint("chat").await.unwrap().is_none());
    let fresh = h.request("open_session", json!({})).await;
    assert_eq!(fresh["result"]["access"]["type"], "read_write", "{fresh}");
    assert_eq!(fresh["result"]["provider_id"], "test:available");
    assert_eq!(fresh["result"]["reasoning_effort"], "low");
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_effort_allows_archived_results_and_reports_archive_errors() {
    let mut h = Harness::new().await;
    h.workspace
        .storage
        .initialize_session(
            "chat",
            SessionModelBinding {
                family: None,
                provider_id: "test".into(),
                model_id: "available".into(),
                reasoning_effort: Some("removed-effort".into()),
            },
        )
        .await
        .unwrap();
    let dir = background_dir(&h.dir.path().join("background"), &h.workspace.id, "chat").unwrap();
    let registry = BackgroundTasks::session_backed(ArchiveDir::open_or_create_root(&dir).unwrap())
        .await
        .unwrap();
    let id = registry
        .spawn_with(
            TaskMeta::shell("unused".into(), "saved".into(), "coda".into()),
            |ctx| async move {
                ctx.append_stdout(b"saved output").await.unwrap();
                TaskExit::Exited { code: Some(0) }
            },
        )
        .await
        .unwrap();
    registry.wait_terminal(&id).await;
    registry.shutdown().await;
    drop(registry);
    let opened = h.request("open_session", json!({})).await;
    assert_eq!(
        opened["result"]["access"]["reason"],
        "reasoning_effort_not_supported"
    );
    assert_eq!(opened["result"]["reasoning_effort"], "removed-effort");
    let result = h.request("get_task_result", json!({"task_id": id})).await;
    assert_eq!(
        result["result"]["output"]["stdout"], "saved output",
        "{result}"
    );
    h.app.relay.detach_all(1).await;
    // A malformed archive must not turn a readable conversation into a failed open.
    std::fs::write(dir.join("unexpected-file"), b"corrupt").unwrap();
    let opened = h.request("open_session", json!({})).await;
    assert_eq!(opened["result"]["access"]["type"], "read_only");
    assert!(
        opened["result"]["background_tasks_error"].is_string(),
        "{opened}"
    );
    h.finish().await;
}

#[path = "model_family.rs"]
mod model_family;
