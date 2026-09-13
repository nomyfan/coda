use super::super::*;
use super::fixtures::*;
use crate::{
    AgentSpec, AgentTeam, ModelProfile, RunConfig,
    runtime::{MemoryStorage, SessionStorage},
};
use coda_core::llm::RequestMessage;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{
    sync::Notify,
    time::{Duration, timeout},
};

#[tokio::test]
async fn exiting_retains_an_envelope_when_snapshot_persistence_fails() {
    let storage = TestStorage::default();
    storage.fail_snapshot_writes(true).await;
    let runtime = ProcessRuntime::new(storage.clone(), "session".into());
    runtime.request_exit().await;
    let reply = Envelope::with_id(|id| Envelope {
        id,
        from: Sender::Agent {
            name: "worker".into(),
            pid: ProcessId::new(),
        },
        to: Receiver {
            name: "coda".into(),
            pid: "session".to_string().into(),
        },
        reply_to: Some("accepted-call".into()),
        body: EnvelopeBody::Reply {
            call_id: "call".into(),
            output: ToolOutput::Ok("done".into()),
            aborted: false,
        },
    });
    runtime.deliver(reply.clone()).await.unwrap();
    assert_eq!(
        runtime.snapshot.lock().await.drained_envelopes["session"][0].id,
        reply.id
    );
    assert!(
        storage
            .load_session_snapshot("session")
            .await
            .unwrap()
            .is_none()
    );
    storage.fail_snapshot_writes(false).await;
    assert!(runtime.wait_for_exit(Some(Duration::from_secs(1))).await);
    assert_eq!(
        storage
            .load_session_snapshot("session")
            .await
            .unwrap()
            .unwrap()
            .drained_envelopes["session"][0]
            .id,
        reply.id
    );
    assert!(runtime.deliver(reply).await.is_err());
}

#[derive(Clone, Default)]
struct DuplicateProvider(Arc<AtomicUsize>);

impl LLMProvider for DuplicateProvider {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl futures::Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        futures::stream::once(async move {
            let prompt = match &request.messages[0] {
                RequestMessage::System(prompt) => prompt.0.as_str(),
                _ => unreachable!(),
            };
            assert_ne!(prompt, "duplicate", "neither duplicate may reach the model");
            let answer = if prompt == "other" {
                self.0.fetch_add(1, Ordering::SeqCst);
                assistant()
            } else if matches!(request.messages.last(), Some(RequestMessage::User(_))) {
                let calls = [
                    ("first", "duplicate", false),
                    ("second", "duplicate", true),
                    ("third", "other", false),
                ]
                .into_iter()
                .map(|(id, target, background)| ToolCall {
                    id: id.into(),
                    name: format!("agent__{target}"),
                    arguments: Some(
                        serde_json::json!({"task":"work", "run_in_background":background})
                            .to_string(),
                    ),
                })
                .collect();
                AssistantMessage {
                    tool_calls: calls,
                    ..assistant()
                }
            } else {
                AssistantMessage {
                    content: "done".into(),
                    ..assistant()
                }
            };
            Ok(LLMStreamEvent::Completed(Box::new(answer)))
        })
    }
}

#[tokio::test]
async fn duplicate_stateful_calls_are_all_rejected_before_any_spawn() {
    let background = Arc::new(coda_execution::BackgroundTasks::temporary().unwrap());
    let root = AgentSpec {
        capabilities: Default::default(),
        name: "coda".into(),
        description: String::new(),
        system_prompt: "root".into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec!["duplicate".into(), "other".into()],
    };
    let children = ["duplicate", "other"]
        .into_iter()
        .map(|name| AgentSpec {
            capabilities: Default::default(),
            name: name.into(),
            description: String::new(),
            system_prompt: name.into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec![],
        })
        .collect();
    let team = AgentTeam::new(root, children).unwrap();
    let provider = DuplicateProvider::default();
    let storage = MemoryStorage::default();
    let pid = ProcessId::new();
    let mut runtime = ProcessRuntime::new(storage.clone(), pid.0.clone());
    runtime.background = Some(background.clone());
    let mut events = runtime.subscribe();
    runtime
        .bootstrap(
            team.build(
                ".",
                coda_tools::shared_file_locks(),
                Some(background.clone()),
            ),
            None,
            HashMap::new(),
            RunConfig {
                default_model: ModelProfile {
                    provider_id: "test".into(),
                    provider: provider.clone(),
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
    runtime.send_message(user_task(&pid, "work")).await.unwrap();
    timeout(Duration::from_secs(3), async {
        loop {
            if matches!(events.recv().await.unwrap().3, AgentEvent::LLMEnd(ref a) if a.content == "done") { break; }
        }
    }).await.unwrap();
    let checkpoint = storage
        .load_checkpoint(pid.as_ref())
        .await
        .unwrap()
        .unwrap();
    for id in ["first", "second"] {
        assert!(checkpoint.messages.iter().any(|entry| matches!(&entry.message,
            Message::Tool(t) if t.id == id && matches!(&t.output, ToolOutput::Err(e) if e.contains("Concurrent invocation")))));
    }
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    assert!(background.summaries().borrow().is_empty());
    runtime.request_exit().await;
    assert!(runtime.wait_for_exit(Some(Duration::from_secs(2))).await);
    background.shutdown().await;
}

#[tokio::test]
async fn reply_after_parent_exit_is_saved_and_consumed_on_reopen() {
    timeout(Duration::from_secs(5), async {
        let storage = MemoryStorage::default();
        let team = AgentTeam::new(AgentSpec {
            capabilities: Default::default(),
            name: "coda".into(), description: String::new(), system_prompt: "main-system".into(),
            mode: SubAgentMode::Stateful, tools: vec![], subagents: vec!["explore".into()],
        }, vec![AgentSpec {
            capabilities: Default::default(),
            name: "explore".into(), description: String::new(), system_prompt: "hold-subagent".into(),
            mode: SubAgentMode::Stateless, tools: vec![], subagents: vec![],
        }]).unwrap();
        let release = Arc::new(Notify::new());
        let harness = Harness::start_agents(storage.clone(), team.build(".", coda_tools::shared_file_locks(), test_registry()),
            TestProvider::with_hold_subagent(release.clone()), ToolApprovalMode::Auto, "start").await;
        loop {
            if let Some(cp) = storage.load_checkpoint(harness.pid.as_ref()).await.unwrap()
                && matches!(cp.resume_point, crate::persist::StoredResumePoint::ToolExecution(ref s) if !s.pending_replies.is_empty()) { break; }
            tokio::task::yield_now().await;
        }
        let mut parent = harness.runtime.processes.lock().await.get(harness.pid.as_ref()).unwrap().finished.clone();
        harness.runtime.request_exit().await;
        while !*parent.borrow_and_update() { parent.changed().await.unwrap(); }
        release.notify_one();
        assert!(harness.runtime.wait_for_exit(Some(Duration::from_secs(2))).await);
        let snapshot = storage.load_session_snapshot(harness.pid.as_ref()).await.unwrap().unwrap();
        assert!(snapshot.drained_envelopes.values().flatten().any(|e| matches!(e.body, EnvelopeBody::Reply { .. })));
        assert!(harness.runtime.send_message(user_task(&harness.pid, "closed")).await.is_err());
        let mut reopened = harness.restart(team.build(".", coda_tools::shared_file_locks(), test_registry()),
            TestProvider::default(), ToolApprovalMode::Auto, HashMap::new()).await;
        loop {
            if matches!(reopened.next_event().await.2, AgentEvent::LLMEnd(ref a) if a.content == "main done") { break; }
        }
        let checkpoint = storage.load_checkpoint(harness.pid.as_ref()).await.unwrap().unwrap();
        assert_eq!(checkpoint.messages.iter().filter(|e| matches!(&e.message, Message::Tool(t) if t.id == "call_explore")).count(), 1);
        reopened.shutdown().await;
    }).await.expect("shutdown reply must survive without restarting its child");
}
