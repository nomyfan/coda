use super::super::super::*;
use super::super::fixtures::{assistant, user_task};
use crate::runtime::MemoryStorage;
use crate::{AgentSpec, AgentTeam, Capabilities, ModelProfile, RunConfig};
use coda_core::llm::RequestMessage;
use coda_execution::{BackgroundTasks, TaskStatus};
use std::sync::Mutex;
use tokio::time::{Duration, timeout};

#[derive(Clone)]
struct Provider {
    background_delegation: bool,
    requests: Arc<Mutex<Vec<ChatCompletionRequest>>>,
}

impl LLMProvider for Provider {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl futures::Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        let RequestMessage::System(prompt) = &request.messages[0] else {
            panic!("system prompt")
        };
        let answered = request
            .messages
            .iter()
            .any(|message| matches!(message, RequestMessage::Tool(_)));
        let mut answer = assistant();
        if answered {
            answer.content = format!("{} done", prompt.0);
        } else if prompt.0 == "root" {
            answer.tool_calls.push(ToolCall {
                id: "delegate".into(),
                name: "agent__worker".into(),
                arguments: Some(
                    serde_json::json!({
                        "task": "work",
                        "run_in_background": self.background_delegation,
                    })
                    .to_string(),
                ),
            });
        } else if self.background_delegation {
            answer.content = "worker done".into();
        } else {
            answer.tool_calls.push(ToolCall {
                id: "shell".into(),
                name: "shell".into(),
                arguments: Some(
                    serde_json::json!({
                        "command": "printf complete",
                        "description": "Produce task output",
                        "run_in_background": true,
                    })
                    .to_string(),
                ),
            });
        }
        self.requests.lock().unwrap().push(request);
        futures::stream::once(async { Ok(LLMStreamEvent::Completed(Box::new(answer))) })
    }
}

async fn start(
    root_capabilities: Capabilities,
    child_capabilities: Capabilities,
    background: Option<Arc<BackgroundTasks>>,
    provider: Provider,
) -> (
    ProcessRuntime,
    tokio::sync::broadcast::Receiver<(String, ProcessId, TurnId, AgentEvent)>,
) {
    let root = AgentSpec {
        capabilities: root_capabilities,
        name: "coda".into(),
        description: String::new(),
        system_prompt: "root".into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec!["worker".into()],
    };
    let child = AgentSpec {
        capabilities: child_capabilities,
        name: "worker".into(),
        description: String::new(),
        system_prompt: "worker".into(),
        mode: SubAgentMode::Stateful,
        tools: vec![Box::new(coda_tools::ShellToolSpec)],
        subagents: vec![],
    };
    let programs = AgentTeam::new(root, vec![child]).unwrap().build(
        ".",
        coda_tools::shared_file_locks(),
        background.clone(),
    );
    let mut runtime = ProcessRuntime::new(MemoryStorage::default(), "capability-session".into());
    runtime.background = background;
    let events = runtime.subscribe();
    runtime
        .bootstrap(
            programs,
            None,
            HashMap::new(),
            RunConfig {
                default_model: ModelProfile {
                    provider_id: "test".into(),
                    provider,
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
    runtime
        .send_message(user_task(
            &ProcessId::from("capability-session".to_string()),
            "start",
        ))
        .await
        .unwrap();
    (runtime, events)
}

#[tokio::test]
async fn root_background_delegation_requires_both_capability_and_registry() {
    timeout(Duration::from_secs(10), async {
        for resource in [false, true] {
            let background = Arc::new(BackgroundTasks::temporary().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let (runtime, mut events) = start(
                if resource {
                    Capabilities::none()
                } else {
                    Capabilities::all()
                },
                Capabilities::all(),
                resource.then(|| background.clone()),
                Provider {
                    background_delegation: true,
                    requests: requests.clone(),
                },
            )
            .await;
            let mut rejected = false;
            loop {
                match events.recv().await.unwrap().3 {
                    AgentEvent::ToolCallEnd(tool) if tool.name == "agent__worker" => {
                        rejected = matches!(
                            &tool.output,
                            ToolOutput::Err(error) if error.contains("background")
                        );
                    }
                    AgentEvent::LLMEnd(answer) if answer.content == "root done" => break,
                    _ => {}
                }
            }
            assert!(rejected);
            assert!(background.summaries().borrow().is_empty());
            {
                let requests = requests.lock().unwrap();
                assert!(requests.iter().all(|request| matches!(
                    &request.messages[0], RequestMessage::System(prompt) if prompt.0 == "root"
                )));
                let delegation = requests[0]
                    .tools
                    .iter()
                    .find(|tool| tool.name == "agent__worker")
                    .unwrap();
                assert!(
                    delegation.parameter_schema["properties"]
                        .get("run_in_background")
                        .is_none()
                );
            }
            runtime.request_exit().await;
            runtime.wait_for_exit(Some(Duration::from_secs(2))).await;
            background.shutdown().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn background_delegation_does_not_require_the_targets_background_capability() {
    timeout(Duration::from_secs(10), async {
        let background = Arc::new(BackgroundTasks::temporary().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (runtime, mut events) = start(
            Capabilities::all(),
            Capabilities::none(),
            Some(background.clone()),
            Provider {
                background_delegation: true,
                requests: requests.clone(),
            },
        )
        .await;
        loop {
            if matches!(
                events.recv().await.unwrap().3,
                AgentEvent::LLMEnd(answer) if answer.content == "root done"
            ) {
                break;
            }
        }
        let task = background.summaries().borrow()[0].id.parse().unwrap();
        background.wait_terminal(&task).await;
        let result = background.read(&task).await.unwrap().unwrap();
        assert!(matches!(result.status, TaskStatus::Completed { .. }));
        assert_eq!(result.stdout, "worker done");
        {
            let requests = requests.lock().unwrap();
            let child = requests
                .iter()
                .find(|request| {
                    matches!(
                        &request.messages[0], RequestMessage::System(prompt) if prompt.0 == "worker"
                    )
                })
                .unwrap();
            assert!(
                child
                    .tools
                    .iter()
                    .all(|tool| tool.name != "task_output" && tool.name != "task_kill")
            );
            let shell = child
                .tools
                .iter()
                .find(|tool| tool.name == "shell")
                .unwrap();
            assert!(
                shell.parameter_schema["properties"]
                    .get("run_in_background")
                    .is_none()
            );
        }
        runtime.request_exit().await;
        runtime.wait_for_exit(Some(Duration::from_secs(2))).await;
        background.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn child_shell_completion_still_notifies_a_root_without_background_capability() {
    timeout(Duration::from_secs(10), async {
        let background = Arc::new(BackgroundTasks::temporary().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (runtime, mut events) = start(
            Capabilities::none(),
            Capabilities::all(),
            Some(background.clone()),
            Provider {
                background_delegation: false,
                requests: requests.clone(),
            },
        )
        .await;
        loop {
            if matches!(
                events.recv().await.unwrap().3,
                AgentEvent::LLMEnd(answer) if answer.content == "root done"
            ) {
                break;
            }
        }
        let task = background.summaries().borrow()[0].id.parse().unwrap();
        background.wait_terminal(&task).await;
        let notices = background.take_notices().await;
        assert_eq!(notices.len(), 1);
        assert!(
            runtime
                .admit_background_notice(
                    "coda".into(),
                    task.clone(),
                    vec![notices[0].outcome()],
                    "child shell finished".into(),
                )
                .await
                .unwrap()
        );
        loop {
            if matches!(
                events.recv().await.unwrap().3,
                AgentEvent::LLMEnd(answer) if answer.content == "root done"
            ) {
                break;
            }
        }
        assert_eq!(
            background.read(&task).await.unwrap().unwrap().stdout,
            "complete"
        );
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| matches!(
                    &request.messages[0], RequestMessage::System(prompt) if prompt.0 == "root"
                ))
                .all(|request| request
                    .tools
                    .iter()
                    .all(|tool| tool.name != "task_output" && tool.name != "task_kill"))
        );
        runtime.request_exit().await;
        runtime.wait_for_exit(Some(Duration::from_secs(2))).await;
        background.shutdown().await;
    })
    .await
    .unwrap();
}
