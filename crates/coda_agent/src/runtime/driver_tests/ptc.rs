use super::super::{AgentToolInvoker, execute_javascript_tool_discovery};
use super::fixtures::*;

use crate::{
    AgentEvent, AgentSpec, SubAgentMode, ToolApprovalMode, ToolCallResolution,
    agent::ToolExecutionMetadata, persist::StoredResumePoint, runtime::SessionStorage,
};
use coda_core::{
    llm::{RequestMessage, ToolOutput},
    tool::{HostToolInvoker, Tools},
};
use coda_tools::{RunJavaScriptToolSpec, ToolSpec, builtin_specs};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::time::{Duration, timeout};

async fn wait_for_final<S>(harness: &mut Harness<S>)
where
    S: SessionStorage + Clone + 'static,
{
    timeout(Duration::from_secs(3), async {
        loop {
            let (agent, _, event) = harness.next_event().await;
            if agent == "coda"
                && let AgentEvent::LLMEnd(message) = event
                && message.tool_calls.is_empty()
            {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for final response");
}

async fn wait_for_suspension<S>(harness: &mut Harness<S>) -> crate::PendingApproval
where
    S: SessionStorage + Clone + 'static,
{
    timeout(Duration::from_secs(3), async {
        loop {
            let (_, _, event) = harness.next_event().await;
            if let AgentEvent::Suspended(approval) = event {
                return approval;
            }
        }
    })
    .await
    .expect("timed out waiting for approval")
}

fn spec(prompt: &str, tools: Vec<Box<dyn ToolSpec>>) -> AgentSpec {
    AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: prompt.into(),
        mode: SubAgentMode::Stateful,
        tools,
        subagents: vec![],
    }
}

#[test]
fn malformed_snapshot_is_deduplicated_and_restored_in_fixed_order() {
    let build = coda_tools::BuildContext::new(".");
    let mut tools = Tools::default();
    tools.register(coda_tools::ShellToolSpec.build(&build));
    tools.register(coda_tools::ListDirectoryToolSpec.build(&build));
    tools.register(coda_tools::ReadTodosToolSpec.build(&build));
    let invoker = AgentToolInvoker::new(
        tools,
        ToolApprovalMode::Auto,
        vec![
            "read_todos".into(),
            "shell".into(),
            "read_todos".into(),
            "unknown".into(),
            "ls".into(),
            "read_todos".into(),
        ],
    );

    assert_eq!(&*invoker.exposed_tools(), &["ls", "read_todos"]);
}

#[tokio::test]
async fn stable_descriptors_are_injected_without_encoding_the_eligible_subset() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let provider = TestProvider::with_recorded_requests(recorded.clone());
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| {
        matches!(call.name.as_str(), "write_file" | "edit_file" | "shell")
    }));
    let mut harness = Harness::start_with_spec(
        crate::runtime::MemoryStorage::default(),
        spec("ptc-descriptor", builtin_specs()),
        provider,
        approval,
        "inspect",
    )
    .await;
    wait_for_final(&mut harness).await;

    {
        let requests = recorded.lock().unwrap();
        let runner = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == coda_tools::RUN_JAVASCRIPT_TOOL_NAME)
            .expect("runner descriptor");
        assert!(runner.description.contains("list_javascript_tools"));
        assert!(!runner.description.contains("tools.read_file("));
        assert!(!runner.description.contains("tools.write_file("));
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME)
        );
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn runner_is_omitted_when_no_bridge_tool_is_auto_approved() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let provider = TestProvider::with_recorded_requests(recorded.clone());
    let mut harness = Harness::start_with_spec(
        crate::runtime::MemoryStorage::default(),
        spec("ptc-descriptor", builtin_specs()),
        provider,
        ToolApprovalMode::Manual,
        "inspect",
    )
    .await;
    wait_for_final(&mut harness).await;

    assert!(
        recorded.lock().unwrap()[0]
            .tools
            .iter()
            .all(|tool| !matches!(
                tool.name.as_str(),
                coda_tools::RUN_JAVASCRIPT_TOOL_NAME | coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME
            ))
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn discovery_uses_the_snapshot_intersected_with_live_policy_and_normal_events() {
    let mut harness = Harness::start_with_spec(
        crate::runtime::MemoryStorage::default(),
        spec(
            "ptc-list",
            vec![
                Box::new(coda_tools::ReadTodosToolSpec),
                Box::new(RunJavaScriptToolSpec),
            ],
        ),
        TestProvider::default(),
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;

    let (saw_start, result) = timeout(Duration::from_secs(3), async {
        let mut saw_start = false;
        let mut result = None;
        loop {
            let (agent, _, event) = harness.next_event().await;
            if agent != "coda" {
                continue;
            }
            match event {
                AgentEvent::ToolCallStart(call)
                    if call.name == coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME =>
                {
                    saw_start = true;
                }
                AgentEvent::ToolCallEnd(tool)
                    if tool.name == coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME =>
                {
                    result = Some(tool);
                }
                AgentEvent::LLMEnd(message) if message.tool_calls.is_empty() => {
                    break (saw_start, result.expect("discovery ToolCallEnd"));
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for discovery");

    assert!(saw_start);
    assert!(matches!(
        result.output,
        ToolOutput::Ok(output) if output == "[read_todos]"
    ));
    assert!(matches!(
        result.outcome,
        coda_core::llm::ToolCallOutcome::Auto
    ));
    assert!(result.started_at.is_some());
    assert!(result.artifacts.is_empty());
    harness.shutdown().await;
}

#[tokio::test]
async fn discovery_requires_exactly_an_empty_object() {
    for input in ["", "null", "[]", "1", r#"{"extra":true}"#, "not-json"] {
        let error = execute_javascript_tool_discovery(input.to_string(), None)
            .await
            .expect_err(input);
        assert!(matches!(
            error,
            coda_core::tool::ToolError::InvalidParameters(_)
        ));
    }

    let build = coda_tools::BuildContext::new(".");
    let mut tools = Tools::default();
    tools.register(coda_tools::ReadTodosToolSpec.build(&build));
    let invoker = AgentToolInvoker::new(tools, ToolApprovalMode::Auto, vec!["read_todos".into()]);
    assert_eq!(
        execute_javascript_tool_discovery(" \n { } \t".to_string(), Some(invoker))
            .await
            .unwrap(),
        "[read_todos]"
    );

    let error = execute_javascript_tool_discovery("{}".to_string(), None)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        coda_core::tool::ToolError::ExecutionError(message)
            if message.contains("PTC_UNAVAILABLE")
    ));
}

#[tokio::test]
async fn runner_executes_nested_builtin_without_intermediate_tool_messages() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let provider = TestProvider::with_recorded_requests(recorded.clone());
    let mut harness = Harness::start_with_spec(
        crate::runtime::MemoryStorage::default(),
        spec(
            "ptc-run",
            vec![
                Box::new(coda_tools::ReadTodosToolSpec),
                Box::new(RunJavaScriptToolSpec),
            ],
        ),
        provider,
        ToolApprovalMode::Auto,
        "inspect",
    )
    .await;
    wait_for_final(&mut harness).await;

    {
        let requests = recorded.lock().unwrap();
        let tool_messages: Vec<_> = requests[1]
            .messages
            .iter()
            .filter_map(|message| match message {
                RequestMessage::Tool(tool) => Some(tool),
                _ => None,
            })
            .collect();
        assert_eq!(tool_messages.len(), 1, "only the outer runner is visible");
        let ToolOutput::Ok(output) = &tool_messages[0].output else {
            panic!("runner failed: {:?}", tool_messages[0].output);
        };
        let report: serde_json::Value = serde_json::from_str(output).unwrap();
        assert_eq!(report["ok"], true);
        assert_eq!(report["value"], "No todos.");
        assert_eq!(report["completed_calls"], 1);
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn outer_approval_preserves_the_generation_snapshot() {
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_spec(
        storage.clone(),
        spec(
            "ptc-run",
            vec![
                Box::new(coda_tools::ReadTodosToolSpec),
                Box::new(RunJavaScriptToolSpec),
            ],
        ),
        TestProvider::default(),
        ToolApprovalMode::RequireWhen(Arc::new(|call| {
            call.name == coda_tools::RUN_JAVASCRIPT_TOOL_NAME
        })),
        "inspect",
    )
    .await;
    let approval = wait_for_suspension(&mut harness).await;

    let checkpoint = storage
        .checkpoint(&harness.thread_id)
        .await
        .expect("suspended checkpoint");
    let StoredResumePoint::PendingApproval {
        pending_approval_calls,
        ..
    } = checkpoint.resume_point
    else {
        panic!("expected pending approval checkpoint");
    };
    assert!(matches!(
        &pending_approval_calls[0].metadata,
        Some(ToolExecutionMetadata::ProgrammaticToolCalling { exposed_tools })
            if exposed_tools == &["read_todos"]
    ));

    harness
        .send_resume(
            &approval,
            vec![(approval.calls[0].id.clone(), ToolCallResolution::Execute)],
        )
        .await;
    wait_for_final(&mut harness).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn discovery_approval_preserves_the_generation_snapshot() {
    let storage = TestStorage::default();
    let mut harness = Harness::start_with_spec(
        storage.clone(),
        spec(
            "ptc-list",
            vec![
                Box::new(coda_tools::ReadTodosToolSpec),
                Box::new(RunJavaScriptToolSpec),
            ],
        ),
        TestProvider::default(),
        ToolApprovalMode::RequireWhen(Arc::new(|call| {
            call.name == coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME
        })),
        "inspect",
    )
    .await;
    let approval = wait_for_suspension(&mut harness).await;

    let checkpoint = storage
        .checkpoint(&harness.thread_id)
        .await
        .expect("suspended checkpoint");
    let StoredResumePoint::PendingApproval {
        pending_approval_calls,
        ..
    } = checkpoint.resume_point
    else {
        panic!("expected pending approval checkpoint");
    };
    assert!(matches!(
        &pending_approval_calls[0].metadata,
        Some(ToolExecutionMetadata::ProgrammaticToolCalling { exposed_tools })
            if exposed_tools == &["read_todos"]
    ));

    harness
        .send_resume(
            &approval,
            vec![(approval.calls[0].id.clone(), ToolCallResolution::Execute)],
        )
        .await;
    wait_for_final(&mut harness).await;
    harness.shutdown().await;
}

#[tokio::test]
async fn live_policy_can_shrink_but_not_bypass_the_snapshot() {
    let read_checks = Arc::new(AtomicUsize::new(0));
    let checks = read_checks.clone();
    let approval = ToolApprovalMode::RequireWhen(Arc::new(move |call| {
        call.name == "read_todos" && checks.fetch_add(1, Ordering::SeqCst) > 0
    }));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut harness = Harness::start_with_spec(
        crate::runtime::MemoryStorage::default(),
        spec(
            "ptc-run",
            vec![
                Box::new(coda_tools::ReadTodosToolSpec),
                Box::new(RunJavaScriptToolSpec),
            ],
        ),
        TestProvider::with_recorded_requests(recorded.clone()),
        approval,
        "inspect",
    )
    .await;
    wait_for_final(&mut harness).await;

    {
        let requests = recorded.lock().unwrap();
        let output = requests[1]
            .messages
            .iter()
            .find_map(|message| match message {
                RequestMessage::Tool(tool) => match &tool.output {
                    ToolOutput::Ok(output) => Some(output),
                    ToolOutput::Err(_) => None,
                },
                _ => None,
            })
            .expect("runner report");
        let report: serde_json::Value = serde_json::from_str(output).unwrap();
        assert_eq!(report["ok"], false);
        assert_eq!(report["error"]["code"], "TOOL_UNAVAILABLE");
        assert_eq!(
            report["error"]["message"],
            "tool \"read_todos\" is unavailable; available tools: []"
        );
    }
    harness.shutdown().await;
}
