use super::super::*;
use super::fixtures::{Harness, assistant, test_config};
use crate::{
    AgentSpec, AgentTeam,
    runtime::{MemoryStorage, SessionStorage},
};
use futures::{StreamExt, stream};
use std::{collections::VecDeque, sync::Mutex};
use tokio::{
    sync::Notify,
    time::{Duration, timeout},
};

#[derive(Clone)]
struct ScriptedProvider {
    scripts: Arc<Mutex<VecDeque<Vec<LLMStreamEvent>>>>,
    stalled: Arc<Notify>,
}

impl LLMProvider for ScriptedProvider {
    fn stream(
        &self,
        _: ChatCompletionRequest,
    ) -> impl futures::Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        let events = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("a script for this generation");
        stream::iter(events.into_iter().map(Ok)).chain(stream::once(async {
            self.stalled.notify_one();
            futures::future::pending().await
        }))
    }
}

async fn start(scripts: Vec<Vec<LLMStreamEvent>>) -> (Harness<MemoryStorage>, Arc<Notify>) {
    let stalled = Arc::new(Notify::new());
    let provider = ScriptedProvider {
        scripts: Arc::new(Mutex::new(scripts.into())),
        stalled: stalled.clone(),
    };
    let agents = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "test".into(),
            mode: SubAgentMode::Stateful,
            capabilities: Default::default(),
            tools: vec![],
            subagents: vec![],
        },
        vec![],
    )
    .unwrap()
    .build(".", coda_tools::shared_file_locks(), None);
    let harness = Harness::start_with_config(
        MemoryStorage::default(),
        agents,
        test_config(provider, ToolApprovalMode::Auto),
        "first",
    )
    .await;
    (harness, stalled)
}

#[tokio::test]
async fn generation_keeps_the_last_report_and_does_not_reuse_it_on_the_next_request() {
    let (mut harness, _) = start(vec![
        vec![
            LLMStreamEvent::ModelReported("first-report".into()),
            LLMStreamEvent::ModelReported("last-report".into()),
            LLMStreamEvent::Completed(Box::new(AssistantMessage {
                content: "first answer".into(),
                ..assistant()
            })),
        ],
        vec![LLMStreamEvent::Completed(Box::new(AssistantMessage {
            content: "second answer".into(),
            ..assistant()
        }))],
    ])
    .await;
    for expected in [Some("last-report"), None] {
        if expected.is_none() {
            harness.send_task("second").await;
        }
        let message = timeout(Duration::from_secs(2), async {
            loop {
                if let (_, _, AgentEvent::LLMEnd(message)) = harness.next_event().await {
                    break message;
                }
            }
        })
        .await
        .unwrap();
        let metadata = message.generation.as_ref().unwrap();
        assert_eq!(metadata.provider_id, "test");
        assert_eq!(metadata.model_id, "fake");
        assert_eq!(metadata.reported_model_id.as_deref(), expected);
        assert!(message.usage.is_none());
        let checkpoint = harness
            .storage
            .load_checkpoint(harness.pid.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert!(
            checkpoint
                .messages
                .iter()
                .any(|entry| matches!(&entry.message, Message::Assistant(saved)
            if saved.message_id == message.message_id && saved.generation == message.generation))
        );
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn cancellation_persists_only_reports_consumed_with_the_partial_generation() {
    for report in [Some("upstream-version"), None] {
        for reasoning_only in [false, true] {
            let mut events = Vec::new();
            if let Some(model) = report {
                events.push(LLMStreamEvent::ModelReported(model.into()));
            }
            events.push(if reasoning_only {
                LLMStreamEvent::ReasoningChunk("partial reasoning".into())
            } else {
                LLMStreamEvent::ContentChunk("partial answer".into())
            });
            let (mut harness, stalled) = start(vec![events]).await;
            timeout(Duration::from_secs(2), stalled.notified())
                .await
                .unwrap();
            harness.runtime.request_abort().await;
            let message = timeout(Duration::from_secs(2), async {
                let mut partial = None;
                loop {
                    match harness.next_event().await.2 {
                        AgentEvent::LLMEnd(message) => partial = Some(message),
                        AgentEvent::Aborted(AbortedTarget::Generation) => break partial.unwrap(),
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            assert!(message.aborted);
            assert_eq!(
                message
                    .generation
                    .as_ref()
                    .unwrap()
                    .reported_model_id
                    .as_deref(),
                report
            );
            assert_eq!(message.reasoning_content.is_some(), reasoning_only);
            let checkpoint = harness
                .storage
                .load_checkpoint(harness.pid.as_ref())
                .await
                .unwrap()
                .unwrap();
            assert!(checkpoint.messages.iter().any(|entry| matches!(&entry.message, Message::Assistant(saved)
                if saved.message_id == message.message_id && saved.generation == message.generation)));
            harness.shutdown().await;
        }
    }
}

#[tokio::test]
async fn cancelling_after_only_a_model_report_does_not_create_a_message() {
    let (mut harness, stalled) = start(vec![vec![LLMStreamEvent::ModelReported(
        "upstream-version".into(),
    )]])
    .await;
    timeout(Duration::from_secs(2), stalled.notified())
        .await
        .unwrap();
    harness.runtime.request_abort().await;
    timeout(Duration::from_secs(2), async {
        loop {
            match harness.next_event().await.2 {
                AgentEvent::LLMEnd(_) => panic!("metadata alone must not create a message"),
                AgentEvent::Aborted(AbortedTarget::Generation) => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let checkpoint = harness
        .storage
        .load_checkpoint(harness.pid.as_ref())
        .await
        .unwrap()
        .unwrap();
    assert!(
        checkpoint
            .messages
            .iter()
            .all(|entry| !matches!(&entry.message, Message::Assistant(_)))
    );
    harness.shutdown().await;
}
