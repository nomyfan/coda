use coda_core::llm::ToolDefinition;
use coda_core::tool::{HostEffectLimits, Tool, ToolCallContext, ToolError, ToolResult};
use serde::Deserialize;
use serde_json::json;

use crate::engine::{JsExecutor, PtcLimits};

pub const RUN_JAVASCRIPT_TOOL_NAME: &str = "run_javascript";
/// Provider-visible synthetic tool used to discover script capabilities.
pub const LIST_JAVASCRIPT_TOOLS_TOOL_NAME: &str = "list_javascript_tools";
/// Maximum UTF-8 size of one capability discovery result.
pub const DISCOVERY_RESULT_BYTES: usize = 16 * 1024;
/// Maximum UTF-8 size of one `TOOL_UNAVAILABLE` message.
pub const TOOL_UNAVAILABLE_MESSAGE_BYTES: usize = 16 * 1024;
pub const PROGRAMMATIC_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "ls",
    "read_todos",
    "write_todos",
];

pub struct RunJavaScriptTool {
    schema: serde_json::Value,
    limits: PtcLimits,
}

impl Default for RunJavaScriptTool {
    fn default() -> Self {
        Self::new(PtcLimits::default())
    }
}

impl RunJavaScriptTool {
    pub fn new(limits: PtcLimits) -> Self {
        Self {
            schema: json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "ES2020 JavaScript body. Top-level await and return are supported."
                    }
                },
                "required": ["code"],
                "additionalProperties": false
            }),
            limits,
        }
    }
}

#[derive(Deserialize)]
pub struct RunJavaScriptParams {
    code: String,
}

impl Tool for RunJavaScriptTool {
    type Parameters = RunJavaScriptParams;
    type Output = String;

    fn name(&self) -> &str {
        RUN_JAVASCRIPT_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Run bounded ES2020 JavaScript with top-level await. Use list_javascript_tools to discover the tools available to the script."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        &self.schema
    }

    fn execute(
        &self,
        params: Self::Parameters,
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let limits = self.limits;
        async move {
            if params.code.len() > limits.source_bytes {
                return Err(ToolError::ResourceLimit(format!(
                    "JavaScript source exceeds {} bytes",
                    limits.source_bytes
                )));
            }
            let invoker = ctx.invoker().ok_or_else(|| {
                ToolError::ExecutionError(
                    "PTC_UNAVAILABLE: run_javascript has no persisted capability snapshot"
                        .to_string(),
                )
            })?;
            let exposed_tools = invoker.exposed_tools();
            if exposed_tools.is_empty() {
                return Err(ToolError::ExecutionError(
                    "PTC_UNAVAILABLE: run_javascript has no persisted capability snapshot"
                        .to_string(),
                ));
            }
            let scope = coda_core::tool::HostCallScope::new(
                ctx.clone(),
                HostEffectLimits {
                    state_bytes: limits.state_bytes,
                    artifact_bytes: limits.artifact_bytes,
                },
            );
            let report = JsExecutor::new(limits)
                .run(
                    params.code,
                    exposed_tools,
                    invoker,
                    scope.clone(),
                    ctx.cancel.clone(),
                )
                .await
                .map_err(map_engine_error)?;
            scope.commit_into_outer()?;
            serde_json::to_string(&report).map_err(|error| {
                ToolError::ExecutionError(format!("failed to serialize JavaScript report: {error}"))
            })
        }
    }
}

fn map_engine_error(error: crate::engine::JsEngineError) -> ToolError {
    match error {
        crate::engine::JsEngineError::Aborted(message) => ToolError::Aborted(message),
        other => ToolError::ExecutionError(other.to_string()),
    }
}

/// Build the stable provider-facing JavaScript runner descriptor.
pub fn run_javascript_definition() -> ToolDefinition {
    let limits = PtcLimits::default();
    ToolDefinition {
        name: RUN_JAVASCRIPT_TOOL_NAME.to_string(),
        description: format!(
            "Run one bounded ES2020 JavaScript program to coordinate several tool calls without returning intermediate results to the model. Call list_javascript_tools before writing the script to discover the currently available tool names; the matching direct tool descriptors in this request contain their parameter schemas. Inside JavaScript, call tools.<name>(input) with exactly one object. Each call returns a Promise<string> containing the tool's raw result; call JSON.parse only when that result is JSON, and never eval tool output. Top-level await and return are supported. Await every tool Promise before returning. Use Promise.all for independent calls expected to succeed, or Promise.allSettled when one call may fail, so no call remains unfinished. A failed tool Promise rejects with a serializable Error whose name, code, and message fields describe the failure; catch it or inspect Promise.allSettled results when failure is expected. TOOL_UNAVAILABLE errors include the tool names still available to this script so it can be adjusted without another discovery call. console.log(...) is the only console method and retains the newest diagnostic output when its limit is exceeded. Return the final compact JSON-serializable value. There are no ambient filesystem, network, process, timer, module, or require APIs; external effects are available only through the discovered tools. Returning with an unfinished tool call produces UNAWAITED_TOOL_CALLS and cancels that call.\n\nRuntime limits:\n{}",
            runtime_limits_description(limits)
        ),
        parameter_schema: json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "ES2020 JavaScript function body with top-level await and return."
                }
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    }
}

/// Build the stable provider-facing capability discovery descriptor.
pub fn list_javascript_tools_definition() -> ToolDefinition {
    ToolDefinition {
        name: LIST_JAVASCRIPT_TOOLS_TOOL_NAME.to_string(),
        description: "List the tool names currently available inside run_javascript. The matching direct tool descriptors in this request contain their parameter schemas. Call with an empty object."
            .to_string(),
        parameter_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

/// Error returned when a provider-visible capability message exceeds its bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityMessageLimitError {
    /// The kind of message that exceeded its byte limit.
    pub resource: &'static str,
    /// The maximum serialized UTF-8 size permitted for the message.
    pub limit_bytes: usize,
}

impl std::fmt::Display for CapabilityMessageLimitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} exceeds its {} byte limit",
            self.resource, self.limit_bytes
        )
    }
}

impl std::error::Error for CapabilityMessageLimitError {}

/// Serialize the ordered names returned by `list_javascript_tools`.
pub fn available_tools_result(available: &[String]) -> Result<String, CapabilityMessageLimitError> {
    let result = json!({ "available_tools": available }).to_string();
    ensure_message_limit(
        result,
        "JavaScript tool discovery result",
        DISCOVERY_RESULT_BYTES,
    )
}

/// Format a bounded error message for a capability rejected by live policy.
pub fn tool_unavailable_message(
    requested: &str,
    available: &[String],
) -> Result<String, CapabilityMessageLimitError> {
    let names = if available.is_empty() {
        "none".to_string()
    } else {
        available.join(", ")
    };
    ensure_message_limit(
        format!("tool \"{requested}\" is unavailable; available tools: {names}"),
        "JavaScript unavailable-tool message",
        TOOL_UNAVAILABLE_MESSAGE_BYTES,
    )
}

fn ensure_message_limit(
    message: String,
    resource: &'static str,
    limit_bytes: usize,
) -> Result<String, CapabilityMessageLimitError> {
    if message.len() > limit_bytes {
        return Err(CapabilityMessageLimitError {
            resource,
            limit_bytes,
        });
    }
    Ok(message)
}

fn runtime_limits_description(limits: PtcLimits) -> String {
    format!(
        "- Submitted source: at most {} of UTF-8 code. QuickJS memory: {}. QuickJS stack: {}.\n\
- Wall-clock deadline: {} seconds, including time waiting for worker capacity, queued calls, and host tool execution; expiry produces DEADLINE_EXCEEDED.\n\
- Tool calls: the first {} calls are admitted; later attempts reject with CALL_LIMIT. At most {} admitted calls execute concurrently; additional calls wait for capacity and still count toward the call total and wall-clock deadline.\n\
- Successful tool results returned to JavaScript: {} of UTF-8 data per call and {} cumulative; excess rejects the affected Promise with RESULT_LIMIT.\n\
- Staged tool effects: {} of thread state and {} of artifacts; excess rejects the affected Promise with RESOURCE_LIMIT.\n\
- console.log: keeps the newest {} of combined output and reports truncation. Final serialized result report: {}, including its JSON envelope but excluding console output; excess produces OUTPUT_LIMIT.",
        format_bytes(limits.source_bytes),
        format_bytes(limits.heap_bytes),
        format_bytes(limits.stack_bytes),
        limits.wall_time.as_secs(),
        limits.max_calls,
        limits.max_concurrent_calls,
        format_bytes(limits.result_bytes),
        format_bytes(limits.total_result_bytes),
        format_bytes(limits.state_bytes),
        format_bytes(limits.artifact_bytes),
        format_bytes(limits.stdout_bytes),
        format_bytes(limits.final_bytes),
    )
}

fn format_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;

    if bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes.is_multiple_of(KIB) {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_failure_after_cancellation_keeps_aborted_semantics() {
        let error = map_engine_error(crate::engine::JsEngineError::Aborted(
            "worker missed teardown grace".to_string(),
        ));

        assert!(matches!(error, ToolError::Aborted(message) if message.contains("teardown")));
    }

    #[tokio::test]
    async fn runner_without_a_snapshot_fails_closed() {
        let error = RunJavaScriptTool::default()
            .execute(
                RunJavaScriptParams {
                    code: "return 1;".to_string(),
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ToolError::ExecutionError(message) if message.contains("PTC_UNAVAILABLE")
        ));
    }

    #[test]
    fn generation_description_exposes_model_relevant_runtime_limits() {
        let limits = PtcLimits::default();
        let description = run_javascript_definition().description;

        for expected in [
            format!(
                "Submitted source: at most {} of UTF-8 code",
                format_bytes(limits.source_bytes)
            ),
            format!("QuickJS memory: {}", format_bytes(limits.heap_bytes)),
            format!("QuickJS stack: {}", format_bytes(limits.stack_bytes)),
            format!(
                "Wall-clock deadline: {} seconds",
                limits.wall_time.as_secs()
            ),
            "queued calls, and host tool execution".to_string(),
            "expiry produces DEADLINE_EXCEEDED".to_string(),
            format!("the first {} calls are admitted", limits.max_calls),
            "later attempts reject with CALL_LIMIT".to_string(),
            format!(
                "At most {} admitted calls execute concurrently",
                limits.max_concurrent_calls
            ),
            "additional calls wait for capacity and still count toward the call total and wall-clock deadline"
                .to_string(),
            format!(
                "Successful tool results returned to JavaScript: {} of UTF-8 data per call and {} cumulative",
                format_bytes(limits.result_bytes),
                format_bytes(limits.total_result_bytes)
            ),
            "excess rejects the affected Promise with RESULT_LIMIT".to_string(),
            format!(
                "Staged tool effects: {} of thread state and {} of artifacts",
                format_bytes(limits.state_bytes),
                format_bytes(limits.artifact_bytes)
            ),
            "excess rejects the affected Promise with RESOURCE_LIMIT".to_string(),
            format!(
                "console.log: keeps the newest {} of combined output",
                format_bytes(limits.stdout_bytes)
            ),
            format!(
                "Final serialized result report: {}",
                format_bytes(limits.final_bytes)
            ),
            "including its JSON envelope but excluding console output".to_string(),
            "excess produces OUTPUT_LIMIT".to_string(),
        ] {
            assert!(
                description.contains(&expected),
                "description omitted {expected:?}: {description}"
            );
        }
    }

    #[test]
    fn provider_descriptors_do_not_depend_on_available_tools() {
        let runner = run_javascript_definition();
        let discovery = list_javascript_tools_definition();

        assert!(!runner.description.contains("tools.read_file"));
        assert!(runner.description.contains(LIST_JAVASCRIPT_TOOLS_TOOL_NAME));
        assert_eq!(discovery.name, LIST_JAVASCRIPT_TOOLS_TOOL_NAME);
        assert_eq!(discovery.parameter_schema["additionalProperties"], false);
    }

    #[test]
    fn capability_messages_preserve_order_and_are_bounded() {
        let names = vec!["read_file".to_string(), "ls".to_string()];

        assert_eq!(
            available_tools_result(&names).unwrap(),
            r#"{"available_tools":["read_file","ls"]}"#
        );
        assert_eq!(
            tool_unavailable_message("write_file", &names).unwrap(),
            r#"tool "write_file" is unavailable; available tools: read_file, ls"#
        );
        assert_eq!(
            tool_unavailable_message("write_file", &[]).unwrap(),
            r#"tool "write_file" is unavailable; available tools: none"#
        );

        let error = ensure_message_limit("12345".to_string(), "test", 4).unwrap_err();
        assert_eq!(error.resource, "test");
        assert_eq!(error.limit_bytes, 4);
        assert_eq!(
            ensure_message_limit("1234".to_string(), "test", 4).unwrap(),
            "1234"
        );

        let oversized = vec!["x".repeat(128); 256];
        assert!(available_tools_result(&oversized).is_err());
        assert!(tool_unavailable_message("read_file", &oversized).is_err());
    }
}
