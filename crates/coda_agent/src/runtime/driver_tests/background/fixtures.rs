use super::super::super::*;
use super::super::fixtures::assistant;
use crate::runtime::{SessionStorage, StoredResumePoint};
use crate::{AgentSpec, AgentTeam, ModelProfile, RunConfig, runtime::MemoryStorage};
use coda_core::llm::RequestMessage;
use coda_process::BackgroundTasks;
use tokio::sync::Notify;

#[derive(Clone, Default)]
pub(super) struct BackgroundProvider {
    pub(super) child_started: Arc<Notify>,
    pub(super) child_release: Arc<Notify>,
    pub(super) approval: bool,
    pub(super) nested_background: bool,
}
impl LLMProvider for BackgroundProvider {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl futures::Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        futures::stream::once(async move {
            let name = match &request.messages[0] {
                RequestMessage::System(s) => s.0.as_str(),
                _ => panic!("system prompt"),
            };
            let answered = request
                .messages
                .iter()
                .any(|m| matches!(m, RequestMessage::Tool(_)));
            let mut answer = assistant();
            match (name, answered) {
                ("root", false) => answer.tool_calls.push(ToolCall {
                    id: "background".into(),
                    name: "agent__worker".into(),
                    arguments: Some(
                        serde_json::json!({"task":"work", "run_in_background":true}).to_string(),
                    ),
                }),
                ("root", true) => answer.content = "root is free".into(),
                ("worker", false) => answer.tool_calls.push(ToolCall {
                    id: "child".into(),
                    name: "agent__child".into(),
                    arguments: Some(serde_json::json!({"task":"child work", "run_in_background":self.nested_background}).to_string()),
                }),
                ("worker", true) => answer.content = "complete final answer".repeat(2000),
                ("child", _) => {
                    self.child_started.notify_one();
                    self.child_release.notified().await;
                    if self.approval && !answered {
                        answer.tool_calls.push(ToolCall {
                            id: "approval".into(),
                            name: "read_todos".into(),
                            arguments: Some("{}".into()),
                        });
                    } else {
                        answer.content = "child answer".into();
                    }
                }
                _ => unreachable!(),
            }
            Ok(LLMStreamEvent::Completed(Box::new(answer)))
        })
    }
}

pub(super) async fn start(
    provider: BackgroundProvider,
) -> (
    AgentRuntime,
    MemoryStorage,
    Arc<BackgroundTasks>,
    tokio::sync::broadcast::Receiver<(String, ThreadId, TurnId, AgentEvent)>,
) {
    let storage = MemoryStorage::default();
    let (runtime, background, events) = start_storage(provider, storage.clone()).await;
    (runtime, storage, background, events)
}

pub(super) async fn start_storage<S: SessionStorage + Clone + 'static>(
    provider: BackgroundProvider,
    storage: S,
) -> (
    AgentRuntime,
    Arc<BackgroundTasks>,
    tokio::sync::broadcast::Receiver<(String, ThreadId, TurnId, AgentEvent)>,
) {
    let background = Arc::new(BackgroundTasks::temporary().unwrap());
    let spec = |name: &str, prompt: &str, subagents: Vec<String>| AgentSpec {
        name: name.into(),
        description: String::new(),
        system_prompt: prompt.into(),
        mode: SubAgentMode::Stateful,
        tools: if name == "child" {
            vec![Box::new(coda_tools::ReadTodosToolSpec)]
        } else {
            vec![]
        },
        subagents,
    };
    let agents = AgentTeam::new(
        spec("coda", "root", vec!["worker".into()]),
        vec![
            spec("worker", "worker", vec!["child".into()]),
            spec("child", "child", vec![]),
        ],
    )
    .unwrap()
    .build(
        ".",
        coda_tools::shared_file_locks(),
        Some(background.clone()),
    );
    let mut runtime = AgentRuntime::new(storage.clone(), "background-session".into());
    runtime.background = Some(background.clone());
    let events = runtime.subscribe();
    runtime
        .bootstrap(
            agents,
            None,
            HashMap::new(),
            RunConfig {
                default_model: ModelProfile {
                    provider,
                    model: "fake".into(),
                    label: "fake".into(),
                    temperature: None,
                    max_completion_tokens: None,
                    reasoning_effort: None,
                    auto_compact_threshold_tokens: u32::MAX,
                },
                agent_models: HashMap::new(),
                tool_approval: ToolApprovalMode::RequireWhen(Arc::new(|call| {
                    call.name == "read_todos"
                })),
                approval_timeout: None,
            },
        )
        .await
        .unwrap();
    (runtime, background, events)
}

#[derive(Clone, Default)]
pub(super) struct FaultStorage {
    pub(super) inner: MemoryStorage,
    pub(super) fail_approval: Arc<std::sync::atomic::AtomicBool>,
    pub(super) block_cleanup: Arc<std::sync::atomic::AtomicBool>,
    pub(super) lose_notice_reply: Arc<std::sync::atomic::AtomicBool>,
    pub(super) receipt_unavailable: Arc<std::sync::atomic::AtomicBool>,
}
impl SessionStorage for FaultStorage {
    fn has_notice_receipt(
        &self,
        task: coda_core::task::TaskId,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<bool, String>> + Send + '_>> {
        Box::pin(async move {
            if self
                .receipt_unavailable
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err("receipt temporarily unavailable".into());
            }
            self.inner.has_notice_receipt(task).await
        })
    }
    fn admit_task_notice(
        &self,
        task: coda_core::task::TaskId,
        checkpoint: crate::StoredCheckpoint,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.inner.admit_task_notice(task, checkpoint).await?;
            if self
                .lose_notice_reply
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.receipt_unavailable
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return Err("commit reply lost".into());
            }
            Ok(())
        })
    }

    fn save_execution_checkpoint(
        &self,
        identity: crate::execution::ExecutionIdentity,
        checkpoint: crate::StoredCheckpoint,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            if checkpoint.agent_name == "child"
                && matches!(
                    checkpoint.resume_point,
                    StoredResumePoint::PendingApproval { .. }
                )
                && self
                    .fail_approval
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err("injected child approval checkpoint failure".into());
            }
            self.inner
                .save_execution_checkpoint(identity, checkpoint)
                .await
        })
    }
    fn abort_scope(
        &self,
        scope: crate::execution::ScopeAbort,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<crate::execution::CleanupReceipt, String>> + Send + '_>,
    > {
        Box::pin(async move {
            if self.block_cleanup.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("injected cleanup outage".into());
            }
            self.inner.abort_scope(scope).await
        })
    }
    fn save_checkpoint(
        &self,
        thread: String,
        checkpoint: crate::StoredCheckpoint,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.save_checkpoint(thread, checkpoint)
    }
    fn load_checkpoint(
        &self,
        thread: &str,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<Option<crate::StoredCheckpoint>, String>> + Send + '_>,
    > {
        self.inner.load_checkpoint(thread)
    }
    fn load_pending_approval_checkpoints(
        &self,
        session: &str,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<Vec<crate::StoredCheckpoint>, String>> + Send + '_>,
    > {
        self.inner.load_pending_approval_checkpoints(session)
    }
    fn load_background_checkpoints(
        &self,
        session: &str,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<Vec<crate::StoredCheckpoint>, String>> + Send + '_>,
    > {
        self.inner.load_background_checkpoints(session)
    }
    fn save_session_snapshot(
        &self,
        session: String,
        snapshot: crate::StoredRuntimeSnapshot,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.save_session_snapshot(session, snapshot)
    }
    fn load_session_snapshot(
        &self,
        session: &str,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<Option<crate::StoredRuntimeSnapshot>, String>> + Send + '_>,
    > {
        self.inner.load_session_snapshot(session)
    }
}
