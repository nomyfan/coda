use super::super::*;
use super::fixtures::*;
use super::ptc::{wait_for_final, wait_for_suspension};
use crate::{
    AgentSpec, AgentTeam, Capabilities, agent::ToolExecutionMetadata, persist::StoredResumePoint,
};
use coda_core::llm::RequestMessage;
use coda_core::tool::ToolObject;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountedTodos(Arc<AtomicUsize>);

impl ToolObject for CountedTodos {
    fn name(&self) -> &str {
        "read_todos"
    }
    fn description(&self) -> &str {
        "Count host executions"
    }
    fn parameter_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({"type": "object"}))
    }
    fn execute(
        self: Arc<Self>,
        _: String,
        _: ToolCallContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolResult<String>> + Send>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("No todos.".into()) })
    }
}

fn spec(prompt: &str, capabilities: Capabilities, calls: Arc<AtomicUsize>) -> AgentSpec {
    AgentSpec {
        capabilities,
        name: "coda".into(),
        description: String::new(),
        system_prompt: prompt.into(),
        mode: SubAgentMode::Stateful,
        tools: vec![Box::new(coda_tools::PrebuiltToolSpec::new(Box::new(
            CountedTodos(calls),
        )))],
        subagents: vec![],
    }
}

#[tokio::test]
async fn disabled_ptc_rejects_both_fabricated_entry_points_without_host_calls() {
    for (prompt, name) in [
        ("ptc-run", "run_javascript"),
        ("ptc-list", "list_javascript_tools"),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut harness = Harness::start_with_spec(
            TestStorage::default(),
            spec(prompt, Capabilities::none(), calls.clone()),
            TestProvider::with_recorded_requests(requests.clone()),
            ToolApprovalMode::Auto,
            "inspect",
        )
        .await;
        wait_for_final(&mut harness).await;
        {
            let requests = requests.lock().unwrap();
            assert!(requests[0].tools.iter().all(|tool| !matches!(
                tool.name.as_str(),
                "run_javascript" | "list_javascript_tools"
            )));
            assert!(requests[1].messages.iter().any(|message| matches!(message,
                RequestMessage::Tool(tool) if tool.name == name && matches!(&tool.output,
                    ToolOutput::Err(error) if error.contains("PTC_UNAVAILABLE")))));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        harness.shutdown().await;
    }
}

#[tokio::test]
async fn restoring_an_approved_ptc_snapshot_cannot_reenable_a_disabled_capability() {
    for (prompt, name) in [
        ("ptc-run", "run_javascript"),
        ("ptc-list", "list_javascript_tools"),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let storage = TestStorage::default();
        let policy = ToolApprovalMode::RequireWhen(Arc::new(move |call| call.name == name));
        let mut harness = Harness::start_with_spec(
            storage.clone(),
            spec(prompt, Capabilities::all(), calls.clone()),
            TestProvider::default(),
            policy.clone(),
            "inspect",
        )
        .await;
        let pending = wait_for_suspension(&mut harness).await;
        let checkpoint = storage.checkpoint(&harness.pid).await.unwrap();
        let StoredResumePoint::PendingApproval {
            pending_approval_calls,
            ..
        } = checkpoint.resume_point
        else {
            panic!("expected an approval checkpoint");
        };
        assert!(matches!(&pending_approval_calls[0].metadata,
            Some(ToolExecutionMetadata::ProgrammaticToolCalling { exposed_tools }) if exposed_tools == &["read_todos"]));
        harness.shutdown().await;

        let programs = AgentTeam::new(spec(prompt, Capabilities::none(), calls.clone()), vec![])
            .unwrap()
            .build(".", coda_tools::shared_file_locks(), test_registry());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let decisions = HashMap::from([(
            "coda".into(),
            (
                pending.pid.clone(),
                crate::ResumeDecision {
                    parent_message_id: pending.parent_message_id,
                    resolutions: vec![(
                        pending.calls[0].id.clone(),
                        crate::ToolCallResolution::Execute,
                    )],
                },
            ),
        )]);
        let mut restored = harness
            .restart(
                programs,
                TestProvider::with_recorded_requests(requests.clone()),
                policy,
                decisions,
            )
            .await;
        wait_for_final(&mut restored).await;
        assert!(
            requests.lock().unwrap()[0]
                .messages
                .iter()
                .any(|message| matches!(message,
            RequestMessage::Tool(tool) if tool.name == name && matches!(&tool.output,
                ToolOutput::Err(error) if error.contains("PTC_UNAVAILABLE"))))
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a restored call reached the host"
        );
        restored.shutdown().await;
    }
}
