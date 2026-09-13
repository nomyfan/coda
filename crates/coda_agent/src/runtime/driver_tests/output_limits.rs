use super::super::*;
use super::fixtures::*;
use crate::{
    AgentSpec, AgentTeam, SubAgentMode,
    runtime::{MemoryStorage, SessionStorage},
};
use coda_core::llm::RequestMessage;
use coda_core::output::{OutputLimits, OutputOwner, OutputRuntime, StorageFailure};
use coda_core::tool::{Tool, ToolResult, ToolWrapper};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
struct BatchProvider(usize);

impl LLMProvider for BatchProvider {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl futures::Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        futures::stream::once(async move {
            let mut response = assistant();
            if request
                .messages
                .iter()
                .any(|m| matches!(m, RequestMessage::Tool(_)))
            {
                response.content = "done".into();
            } else {
                response.tool_calls = (0..self.0)
                    .map(|i| ToolCall {
                        id: format!("call-{i}"),
                        name: "effect".into(),
                        arguments: Some("{}".into()),
                        output_bytes: None,
                    })
                    .collect();
            }
            Ok(LLMStreamEvent::Completed(Box::new(response)))
        })
    }
}

struct EffectTool {
    calls: Arc<AtomicUsize>,
    mode: &'static str,
    schema: Value,
}

impl Tool for EffectTool {
    type Parameters = Value;
    type Output = OutputData;
    fn name(&self) -> &str {
        "effect"
    }
    fn description(&self) -> &str {
        "Record a state effect and return output."
    }
    fn parameter_schema(&self) -> &Value {
        &self.schema
    }
    fn execute(
        &self,
        _: Value,
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<OutputData>> + Send + 'static {
        let calls = self.calls.clone();
        let mode = self.mode;
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            ctx.state.set("effect", serde_json::json!(true))?;
            Ok(match mode {
                "unavailable" => OutputData::unavailable(
                    "successful execution diagnostic".into(),
                    StorageFailure::Io,
                ),
                "invalid_page" => OutputData::Page {
                    body: "x".repeat(100_000),
                    references: vec![],
                },
                _ => "中间日志".repeat(20_000).into(),
            })
        }
    }
}

#[tokio::test]
async fn batch_delivery_bounds_output_and_preserves_only_delivered_effects() {
    for (mode, count) in [
        ("large", 8),
        ("unavailable", 1),
        ("invalid_page", 1),
        ("large", 128),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            coda_output::Store::open(OutputLimits {
                root: dir.path().join("output"),
                ..OutputLimits::default()
            })
            .unwrap(),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let spec = AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "output test".into(),
            mode: SubAgentMode::Stateful,
            capabilities: Default::default(),
            subagents: vec![],
            tools: vec![Box::new(coda_tools::PrebuiltToolSpec::new(Box::new(
                ToolWrapper::from(EffectTool {
                    calls: calls.clone(),
                    mode,
                    schema: serde_json::json!({"type":"object"}),
                }),
            )))],
        };
        let agents =
            AgentTeam::new(spec, vec![])
                .unwrap()
                .build(".", coda_tools::shared_file_locks(), None);
        let mut config = test_config(BatchProvider(count), ToolApprovalMode::Auto);
        config.outputs = Some(OutputRuntime {
            store,
            owner: OutputOwner {
                workspace_id: "test".into(),
                session_id: "test".into(),
            },
            ptc: Default::default(),
        });
        let mut harness =
            Harness::start_with_config(MemoryStorage::default(), agents, config, "run").await;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let (_, _, event) = harness.next_event().await;
                if matches!(event, AgentEvent::Error(_))
                    || matches!(event, AgentEvent::LLMEnd(ref m) if m.tool_calls.is_empty())
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        harness.shutdown().await;
        let checkpoint = harness
            .storage
            .load_checkpoint(harness.pid.as_ref())
            .await
            .unwrap()
            .unwrap();
        let results: Vec<_> = checkpoint
            .messages
            .iter()
            .filter_map(|entry| match &entry.message {
                Message::Tool(tool) => Some((tool, &entry.state)),
                _ => None,
            })
            .collect();
        if count == 128 {
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(results.is_empty());
            continue;
        }
        assert_eq!(calls.load(Ordering::SeqCst), count);
        assert_eq!(results.len(), count);
        let mut total = 0;
        for (tool, state) in results {
            let text = match &tool.output {
                ToolOutput::Ok(text) | ToolOutput::Err(text) => text,
            };
            total += text.len();
            assert!(text.len() <= (65536 / count).min(16384));
            assert_eq!(state.is_empty(), mode == "invalid_page");
            if mode == "large" {
                assert_eq!(tool.output_refs.len(), 1);
                assert_eq!(
                    std::fs::read_to_string(&tool.output_refs[0].channels[0].path).unwrap(),
                    "中间日志".repeat(20_000)
                );
            } else if mode == "unavailable" {
                assert!(matches!(tool.output, ToolOutput::Ok(_)));
                assert!(text.contains("successful execution diagnostic"));
                assert!(tool.output_refs.is_empty());
            } else {
                assert!(matches!(tool.output, ToolOutput::Err(_)));
            }
        }
        assert!(total <= 65536);
    }
}
