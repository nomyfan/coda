use super::super::super::*;
use super::super::fixtures::{assistant, user_task};
use super::fixtures::start_storage;
use crate::runtime::{MemoryStorage, SessionStorage};
use coda_core::llm::RequestMessage;
use coda_execution::{TaskExit, TaskId, TaskKind, TaskMeta, TaskNotice};
use tokio::time::{Duration, timeout};

#[derive(Clone, Default)]
struct TaskReader {
    tasks: Arc<std::sync::Mutex<Vec<TaskId>>>,
}

impl LLMProvider for TaskReader {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl futures::Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        futures::stream::once(async move {
            let RequestMessage::System(prompt) = &request.messages[0] else {
                panic!("system prompt")
            };
            let answered = request
                .messages
                .iter()
                .any(|m| matches!(m, RequestMessage::Tool(_)));
            let mut answer = assistant();
            match (prompt.0.as_str(), answered) {
                ("root", false) => answer.tool_calls.push(ToolCall {
                    id: "background".into(),
                    name: "agent__worker".into(),
                    arguments: Some(
                        serde_json::json!({"task":"read tasks", "run_in_background":true})
                            .to_string(),
                    ),
                }),
                ("worker", false) => {
                    for (index, id) in self.tasks.lock().unwrap().iter().enumerate() {
                        answer.tool_calls.push(ToolCall {
                            id: format!("read-{index}"),
                            name: "task_output".into(),
                            arguments: Some(serde_json::json!({"id":id}).to_string()),
                        });
                    }
                }
                _ => answer.content = "done".into(),
            }
            Ok(LLMStreamEvent::Completed(Box::new(answer)))
        })
    }
}

#[tokio::test]
async fn subagent_acknowledges_only_its_own_completed_shell_reads() {
    timeout(Duration::from_secs(10), async {
        let provider = TaskReader::default();
        let storage = MemoryStorage::default();
        let (runtime, background, _) = start_storage(provider.clone(), storage.clone()).await;
        let root = ProcessId::from("background-session".to_string());
        let worker = ProcessId::from_uuid5(&root, "worker");
        let mut ids = Vec::new();
        for (owner, subagent, running) in [
            (worker.as_ref(), false, false),
            ("another-process", false, false),
            (worker.as_ref(), true, false),
            (worker.as_ref(), false, true),
        ] {
            let mut meta = TaskMeta::shell("test".into(), "test".into(), "worker".into());
            meta.origin.pid = owner.into();
            if subagent {
                meta.kind = TaskKind::Subagent { agent_name: "other".into() };
            }
            let id = background.spawn_with(meta, move |ctx| async move {
                if running {
                    ctx.cancelled().cancelled().await;
                    TaskExit::Killed
                } else if subagent {
                    TaskExit::Completed { answer: "answer".into() }
                } else {
                    ctx.append_stdout(b"complete shell result").await.unwrap();
                    TaskExit::Exited { code: Some(0) }
                }
            }).await.unwrap();
            if !running {
                background.wait_terminal(&id).await;
            }
            ids.push(id);
        }
        *provider.tasks.lock().unwrap() = ids.clone();
        runtime.send_message(user_task(&root, "start")).await.unwrap();
        // The parent task's terminal commit follows the worker's final checkpoint.
        let parent = loop {
            let parent = background.summaries().borrow().iter()
                .find(|s| matches!(&s.kind, TaskKind::Subagent { agent_name } if agent_name == "worker"))
                .map(|s| s.id.parse::<TaskId>().unwrap());
            if let Some(id) = parent { break id; }
            tokio::task::yield_now().await;
        };
        background.wait_terminal(&parent).await;
        assert!(storage.has_notice_receipt(ids[0].clone()).await.unwrap());
        for id in &ids[1..] {
            assert!(!storage.has_notice_receipt(id.clone()).await.unwrap());
        }
        assert!(!storage.has_notice_receipt(parent.clone()).await.unwrap(),
            "reading a child shell must not acknowledge its parent agent");
        let checkpoint = storage.load_checkpoint(worker.as_ref()).await.unwrap().unwrap();
        assert!(checkpoint.messages.iter().any(|entry| matches!(&entry.message,
            Message::Tool(tool) if tool.observed_task.as_ref() == Some(&ids[0]))));
        // The running shell outlives its owner; its later completion remains unacknowledged.
        background.kill(&ids[3]).await.unwrap();
        assert!(!storage.has_notice_receipt(ids[3].clone()).await.unwrap());
        let notices = background.take_notices().await;
        assert!(notices.iter().any(|n| matches!(n, TaskNotice::Task { id, .. } if id == &ids[3])));
        assert!(notices.iter().any(|n| matches!(n, TaskNotice::Subagent { id, .. } if id == &parent)));
        runtime.request_exit().await;
        assert!(runtime.wait_for_exit(Some(Duration::from_secs(2))).await);
        background.shutdown().await;
    }).await.unwrap();
}
