use coda_core::llm::ToolDefinition;
use coda_core::tool::{HostEffectLimits, Tool, ToolCallContext, ToolError, ToolResult};
use serde::Deserialize;
use serde_json::json;

use crate::engine::{JsExecutor, PtcLimits};

pub const RUN_JAVASCRIPT_TOOL_NAME: &str = "run_javascript";
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
        "Run bounded ES2020 JavaScript with top-level await. The available tools are listed dynamically in this tool's request description."
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
                    "run_javascript was started without a host tool invoker".to_string(),
                )
            })?;
            let exposed_tools = invoker.exposed_tools();
            if exposed_tools.is_empty() {
                return Err(ToolError::ExecutionError(
                    "run_javascript has no persisted capability snapshot".to_string(),
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

/// Build the generation-specific descriptor from the exact eligible subset.
pub fn run_javascript_definition(available: &[ToolDefinition]) -> ToolDefinition {
    let mut api_lines = Vec::with_capacity(available.len());
    for tool in available {
        let schema = serde_json::to_string(&tool.parameter_schema)
            .unwrap_or_else(|_| "{\"type\":\"object\"}".to_string());
        api_lines.push(format!(
            "- tools.{}(input: {}) -> Promise<string>",
            tool.name, schema
        ));
    }
    ToolDefinition {
        name: RUN_JAVASCRIPT_TOOL_NAME.to_string(),
        description: format!(
            "Run one bounded ES2020 JavaScript program to coordinate several tool calls without returning intermediate results to the model. Top-level await and return are supported. Await every tool Promise before returning. Use Promise.all for independent calls expected to succeed, or Promise.allSettled when one call may fail, so no call remains unfinished. Each API takes exactly one object and resolves to the tool's raw string result; call JSON.parse only when that result is JSON, and never eval tool output. A failed tool Promise rejects with a serializable Error whose name, code, and message fields describe the failure; catch it or inspect Promise.allSettled results when failure is expected. console.log(...) is the only console method and is available as bounded diagnostic output. Return the final compact JSON-serializable value. There are no ambient filesystem, network, process, timer, module, or require APIs; external effects are available only through the APIs below. A capability may reject with TOOL_UNAVAILABLE if policy tightens while the script runs. Returning with an unfinished tool call produces UNAWAITED_TOOL_CALLS and cancels that call.\n\nAvailable APIs:\n{}",
            api_lines.join("\n")
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
}
