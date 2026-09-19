//! Thin tool shells over the `coda_execution` task registry: `task_output` reads
//! incremental output, `task_kill` terminates a task. Both are buildable for
//! any agent that is granted them (the registry handle is always present in
//! the build context) — they are not tied to `shell`.

use std::str::FromStr;
use std::sync::Arc;

use coda_core::tool::{Tool, ToolCallContext, ToolResult};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};

use coda_execution::{BackgroundTasks, TaskId};

fn unknown_task(id: &str) -> String {
    format!(
        "Unknown or expired task id: {id}. Retained output is no longer available; \
         completion notices contain task metadata, not output."
    )
}

/// Parse a model-supplied id into a validated [`TaskId`], or return the tool
/// error text for a malformed one (which can never name an archive path).
fn parse_id(raw: &str) -> Result<TaskId, String> {
    TaskId::from_str(raw).map_err(|_| {
        format!("Invalid task id: {raw}. Expected an id like \"bg_...\" as returned when the task started.")
    })
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TaskOutputToolParams {
    /// The background task id, as returned when the task was started
    /// (e.g. "bg_1234...").
    id: String,
    /// Continue a subagent answer from this byte offset. Shell output is incremental per caller.
    byte_offset: Option<u64>,
}

pub struct TaskOutputTool {
    schema: Schema,
    background: Arc<BackgroundTasks>,
}

impl TaskOutputTool {
    pub fn new(background: Arc<BackgroundTasks>) -> Self {
        TaskOutputTool {
            schema: schemars::schema_for!(TaskOutputToolParams),
            background,
        }
    }
}

impl Tool for TaskOutputTool {
    type Parameters = TaskOutputToolParams;
    type Output = coda_core::output::OutputData;

    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Read a background task: its status, then new stdout/stderr for a shell task or the final answer for a subagent. Output longer than the page ends with a truncation note: call again for more shell output (reads advance per caller), or pass the byte_offset it names to continue a subagent answer, which otherwise starts from the beginning and can be read repeatedly. Reading saved files directly or through the dashboard does not mark the task as read."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let background = self.background.clone();
        async move {
            let id = match parse_id(&params.id) {
                Ok(id) => id,
                Err(msg) => return Ok(msg.into()),
            };
            use coda_core::output::{Channel, HostResultBuffer, OutputData};
            let bytes = ctx.result_budget.page_bytes();
            let lease = ctx.result_budget.reserve(bytes * 2, &ctx.cancel).await?;
            let mut positions = [0; 3];
            for (index, channel) in [Channel::Stdout, Channel::Stderr, Channel::Result]
                .into_iter()
                .enumerate()
            {
                positions[index] = ctx.read_offset(
                    &id,
                    channel,
                    background
                        .output_progress(&ctx.origin.pid, &id, channel)
                        .await,
                );
            }
            let page = match background
                .read_page(&id, &ctx.origin.pid, positions, params.byte_offset, bytes)
                .await
            {
                Ok(Some(page)) => page,
                Ok(None) => return Ok(unknown_task(&params.id).into()),
                Err(error) => {
                    return Err(coda_core::tool::ToolError::ExecutionError(
                        error.to_string(),
                    ));
                }
            };
            if page.complete {
                ctx.record_task_result(id);
            }
            ctx.record_reads(page.receipts);
            if lease.is_some() {
                Ok(OutputData::Buffered(HostResultBuffer {
                    text: page.body,
                    lease,
                }))
            } else {
                Ok(OutputData::Page {
                    body: page.body,
                    references: page.references,
                })
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TaskKillToolParams {
    /// The background task id, as returned when the task was started
    /// (e.g. "bg_1234...").
    id: String,
}

pub struct TaskKillTool {
    schema: Schema,
    background: Arc<BackgroundTasks>,
}

impl TaskKillTool {
    pub fn new(background: Arc<BackgroundTasks>) -> Self {
        TaskKillTool {
            schema: schemars::schema_for!(TaskKillToolParams),
            background,
        }
    }
}

impl Tool for TaskKillTool {
    type Parameters = TaskKillToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "task_kill"
    }

    fn description(&self) -> &str {
        "Stop a background task and the work it owns, including synchronous \
         subagents and background shell processes. Repeated stops are harmless."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let background = self.background.clone();
        async move {
            let id = match parse_id(&params.id) {
                Ok(id) => id,
                Err(msg) => return Ok(msg),
            };
            let result = if ctx.background_task.as_ref() == Some(&id) {
                background.request_kill(&id).await
            } else {
                background.kill(&id).await
            };
            match result {
                Ok(None) => Ok(unknown_task(&params.id)),
                Ok(Some(status)) => Ok(format!("Task {}: {}.", params.id, status.describe())),
                Err(e) => Err(coda_core::tool::ToolError::ExecutionError(e.to_string())),
            }
        }
    }
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
