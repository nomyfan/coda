use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const CHILDREN: usize = 32;

#[derive(Clone, Default)]
struct BurstProvider(Arc<AtomicUsize>);

impl coda_core::llm::LLMProvider for BurstProvider {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        stream::once(async move {
            let worker =
                matches!(&request.messages[0], RequestMessage::System(s) if s.0 == "worker");
            let answer = if worker {
                self.0.fetch_add(1, Ordering::SeqCst);
                AssistantMessage {
                    content: "child done".into(),
                    ..assistant()
                }
            } else if matches!(request.messages.last(), Some(RequestMessage::User(_))) {
                AssistantMessage {
                    tool_calls: (0..CHILDREN)
                        .map(|id| ToolCall {
                            id: format!("child-{id}"),
                            name: "agent__worker".into(),
                            arguments: Some(json!({"task": "work"}).to_string()),
                        })
                        .collect(),
                    ..assistant()
                }
            } else {
                let replies = request.messages.iter().filter(|message| {
                    matches!(message, RequestMessage::Tool(tool) if matches!(&tool.output, ToolOutput::Ok(text) if text == "child done"))
                }).count();
                assert_eq!(
                    replies, CHILDREN,
                    "every child must reply before the root continues"
                );
                AssistantMessage {
                    content: "root done".into(),
                    ..assistant()
                }
            };
            Ok(LLMStreamEvent::Completed(Box::new(answer)))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replies_exceeding_inbox_capacity_do_not_block_dispatch_or_shutdown() {
    let root = AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: "root".into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec!["worker".into()],
    };
    let child = AgentSpec {
        name: "worker".into(),
        description: String::new(),
        system_prompt: "worker".into(),
        mode: SubAgentMode::Stateless,
        tools: vec![],
        subagents: vec![],
    };
    let team = AgentTeam::new(root, vec![child]).unwrap();
    let provider = BurstProvider::default();
    let session = Session::builder()
        .team(&team, ".")
        .background(None)
        .storage(MemoryStorage::default())
        .run_config(RunConfig {
            default_model: ModelProfile {
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
        })
        .open()
        .await
        .unwrap();
    session
        .send(MessageId::new(), "start", vec![])
        .await
        .unwrap();
    let completed = timeout(Duration::from_secs(3), async {
        while let Some(item) = session.recv().await {
            if matches!(item, SessionStreamItem::Event(e) if matches!(e.kind, AgentEvent::LLMEnd(ref a) if a.content == "root done")) {
                return;
            }
        }
        panic!("session ended without the root answer");
    }).await;
    let stopped = timeout(
        Duration::from_secs(4),
        session.shutdown(Shutdown::graceful_then_abort(Duration::from_millis(100))),
    )
    .await;
    assert!(
        stopped.is_ok(),
        "backpressure must not block shutdown before its deadline starts"
    );
    assert!(
        completed.is_ok(),
        "only {} children started",
        provider.0.load(Ordering::SeqCst)
    );
    assert_eq!(provider.0.load(Ordering::SeqCst), CHILDREN);
}
