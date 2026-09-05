use super::super::*;
use super::fixtures::{assistant, user_task};
use crate::{AgentSpec, AgentTeam, ModelProfile, RunConfig, runtime::MemoryStorage};
use coda_core::llm::RequestMessage;
use tokio::sync::Barrier;
use tokio::time::{Duration, timeout};

#[derive(Clone)]
struct ParallelProvider(Arc<Barrier>, bool);

impl LLMProvider for ParallelProvider {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl futures::Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        futures::stream::once(async move {
            let worker = matches!(&request.messages[0], RequestMessage::System(prompt) if prompt.0 == "worker");
            let reply = if worker {
                // Neither invocation can finish before the other has started.
                self.0.wait().await;
                let task = request
                    .messages
                    .iter()
                    .find_map(|message| match message {
                        RequestMessage::User(user) => Some(user.parts.clone()),
                        _ => None,
                    })
                    .unwrap();
                AssistantMessage {
                    content: format!("finished {task:?}"),
                    ..assistant()
                }
            } else if matches!(request.messages.last(), Some(RequestMessage::User(_))) {
                AssistantMessage {
                    tool_calls: ["first", "second"]
                        .into_iter()
                        .map(|task| ToolCall {
                            id: task.into(),
                            name: "agent__worker".into(),
                            arguments: Some(
                                serde_json::json!({"task":task, "run_in_background":self.1})
                                    .to_string(),
                            ),
                        })
                        .collect(),
                    ..assistant()
                }
            } else {
                assert_eq!(
                    request
                        .messages
                        .iter()
                        .filter(|m| matches!(m, RequestMessage::Tool(_)))
                        .count()
                        % 2,
                    0
                );
                AssistantMessage {
                    content: "both finished".into(),
                    ..assistant()
                }
            };
            Ok(LLMStreamEvent::Completed(Box::new(reply)))
        })
    }
}

async fn parallel_invocations(background_enabled: bool) {
    let background =
        background_enabled.then(|| Arc::new(coda_process::BackgroundTasks::temporary().unwrap()));
    let storage = MemoryStorage::default();
    let root = ThreadId::from("parallel-session".to_string());
    let agents = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "root".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec!["worker".into()],
        },
        vec![AgentSpec {
            name: "worker".into(),
            description: String::new(),
            system_prompt: "worker".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![],
            subagents: vec![],
        }],
    )
    .unwrap()
    .build(".", coda_tools::shared_file_locks(), background.clone());
    let mut runtime = AgentRuntime::new(storage.clone(), root.as_ref().into());
    runtime.background = background.clone();
    let mut events = runtime.subscribe();
    runtime
        .bootstrap(
            agents,
            None,
            HashMap::new(),
            RunConfig {
                default_model: ModelProfile {
                    provider: ParallelProvider(Arc::new(Barrier::new(2)), background_enabled),
                    model: "fake".into(),
                    label: "fake".into(),
                    temperature: None,
                    max_completion_tokens: None,
                    reasoning_effort: None,
                    auto_compact_threshold_tokens: u32::MAX,
                },
                agent_models: HashMap::new(),
                tool_approval: ToolApprovalMode::Auto,
                approval_timeout: None,
            },
        )
        .await
        .unwrap();
    let turns = if background_enabled { 1 } else { 16 };
    for _ in 0..turns {
        runtime
            .send_message(user_task(&root, "run both"))
            .await
            .unwrap();
        timeout(Duration::from_secs(2), async {
        loop {
            let (_, thread, _, event) = events.recv().await.unwrap();
            if thread == root && matches!(event, AgentEvent::LLMEnd(ref answer) if answer.content == "both finished") {
                break;
            }
        }
    }).await.expect("both calls must run before either answers");
    }
    if let Some(background) = &background {
        let ids: Vec<coda_core::task::TaskId> = background
            .summaries()
            .borrow()
            .iter()
            .map(|s| s.id.parse().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        for id in ids {
            background.wait_terminal(&id).await;
        }
    }
    if !background_enabled {
        assert_eq!(
            runtime.agents.lock().await.len(),
            1,
            "only the root driver stays live"
        );
        assert_eq!(
            runtime.executions.lock().unwrap().threads.len(),
            1,
            "completed invocations release their execution records"
        );
    }
    if !background_enabled {
        assert!(
            runtime.agent_tasks.lock().unwrap().len() <= 5,
            "completed JoinSet entries must not accumulate across turns"
        );
        assert!(
            runtime
                .snapshot
                .lock()
                .await
                .agent_drained_envelopes
                .is_empty()
        );
    }
    runtime.request_exit().await;
    assert!(runtime.wait_for_exit(Some(Duration::from_secs(2))).await);
    let workers: Vec<_> = storage
        .all_checkpoints()
        .await
        .into_iter()
        .filter(|checkpoint| checkpoint.agent_name == "worker")
        .collect();
    assert_eq!(workers.len(), turns * 2);
    assert_eq!(
        workers
            .iter()
            .map(|worker| &worker.thread_id)
            .collect::<HashSet<_>>()
            .len(),
        turns * 2
    );
    assert_ne!(workers[0].thread_id, workers[1].thread_id);
    for worker in workers {
        assert_eq!(worker.messages.len(), 2);
        assert_eq!(worker.parent_thread_id.as_deref(), Some(root.as_ref()));
    }
}

#[tokio::test]
async fn same_stateless_agent_runs_in_parallel_with_isolated_histories() {
    parallel_invocations(false).await;
}
#[tokio::test]
async fn same_stateless_background_agent_runs_in_parallel_with_isolated_histories() {
    parallel_invocations(true).await;
}
