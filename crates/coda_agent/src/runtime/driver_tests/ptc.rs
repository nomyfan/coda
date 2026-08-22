use super::super::AgentToolInvoker;
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
fn malformed_snapshot_cannot_expand_beyond_the_fixed_ptc_tools() {
    let build = coda_tools::BuildContext::new(".");
    let mut tools = Tools::default();
    tools.register(coda_tools::ShellToolSpec.build(&build));
    tools.register(coda_tools::ReadTodosToolSpec.build(&build));
    let invoker = AgentToolInvoker::new(
        tools,
        ToolApprovalMode::Auto,
        vec!["shell".into(), "read_todos".into(), "unknown".into()],
    );

    assert_eq!(&*invoker.exposed_tools(), &["read_todos"]);
}

#[tokio::test]
async fn descriptor_lists_only_the_generation_eligible_subset() {
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
        for name in ["read_file", "ls", "read_todos", "write_todos"] {
            assert!(runner.description.contains(&format!("tools.{name}(")));
        }
        for name in ["write_file", "edit_file"] {
            assert!(!runner.description.contains(&format!("tools.{name}(")));
        }
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
            .all(|tool| tool.name != coda_tools::RUN_JAVASCRIPT_TOOL_NAME)
    );
    harness.shutdown().await;
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
        assert!(
            report["error"]["message"]
                .as_str()
                .unwrap()
                .contains("TOOL_UNAVAILABLE")
        );
    }
    harness.shutdown().await;
}
