//! Thin tool shells over the `coda_process` task registry: `task_output` reads
//! incremental output, `task_kill` terminates a task. Both are buildable for
//! any agent that is granted them (the registry handle is always present in
//! the build context) — they are not tied to `shell`.

use std::str::FromStr;
use std::sync::Arc;

use coda_core::tool::{Tool, ToolCallContext, ToolResult};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};

use coda_process::{BackgroundTasks, TaskId};

fn unknown_task(id: &str) -> String {
    format!(
        "Unknown or expired task id: {id}. Finished tasks are reclaimed \
         after a while; their final output was delivered in the completion \
         notice."
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
    type Output = String;

    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Read a background task. Shell output is incremental; subagent results \
         include the complete final answer and can be read repeatedly."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        _ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let background = self.background.clone();
        async move {
            let id = match parse_id(&params.id) {
                Ok(id) => id,
                Err(msg) => return Ok(msg),
            };
            let read = match background.read(&id).await {
                Ok(Some(read)) => read,
                Ok(None) => return Ok(unknown_task(&params.id)),
                Err(e) => return Err(coda_core::tool::ToolError::ExecutionError(e.to_string())),
            };
            let mut out = format!("status: {}", read.status.describe());
            if let Some(note) = &read.note {
                out.push_str(&format!("\n({note})"));
                return Ok(out);
            }
            if read.stdout_lost > 0 {
                out.push_str(&format!(
                    "\n({} bytes of stdout were overwritten before they could be read)",
                    read.stdout_lost
                ));
            }
            if !read.stdout.is_empty() {
                out.push_str(&format!("\nstdout (new):\n{}", read.stdout));
            }
            if read.stderr_lost > 0 {
                out.push_str(&format!(
                    "\n({} bytes of stderr were overwritten before they could be read)",
                    read.stderr_lost
                ));
            }
            if !read.stderr.is_empty() {
                out.push_str(&format!("\nstderr (new):\n{}", read.stderr));
            }
            if read.stdout.is_empty()
                && read.stderr.is_empty()
                && read.stdout_lost == 0
                && read.stderr_lost == 0
            {
                out.push_str("\n(no new output)");
            }
            Ok(out)
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
mod tests {
    use super::*;
    use coda_process::TaskMeta;
    use tokio::process::Command;

    fn bash(command: &str) -> Command {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(command);
        cmd
    }

    fn meta(command: &str) -> TaskMeta {
        TaskMeta::shell(command.into(), "test task".into(), "coda".into())
    }

    #[tokio::test]
    async fn a_subagent_can_request_its_own_stop_without_waiting_for_itself() {
        let background = Arc::new(BackgroundTasks::temporary().unwrap());
        let id = coda_process::TaskId::new();
        let own_id = id.clone();
        let tool = TaskKillTool::new(background.clone());
        let meta = coda_process::TaskMeta {
            kind: coda_process::TaskKind::Subagent {
                agent_name: "worker".into(),
            },
            description: "self stop".into(),
            parent_task_id: None,
            origin: Default::default(),
        };
        background
            .spawn_identified(id.clone(), meta, move |_| async move {
                let mut ctx = ToolCallContext::default();
                ctx.background_task = Some(own_id.clone());
                tool.execute(
                    TaskKillToolParams {
                        id: own_id.to_string(),
                    },
                    ctx,
                )
                .await
                .unwrap();
                coda_process::TaskExit::Killed
            })
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            background.wait_terminal(&id),
        )
        .await
        .expect("self stop must not wait on its own monitor");
        assert!(matches!(
            background.read(&id).await.unwrap().unwrap().status,
            coda_process::TaskStatus::Killed { .. }
        ));
    }

    #[tokio::test]
    async fn task_output_reads_incrementally_and_reports_expiry() {
        let background = Arc::new(BackgroundTasks::temporary().unwrap());
        let id = background
            .spawn(bash("echo first; sleep 39.01"), meta("stream"))
            .await
            .unwrap();
        let tool = TaskOutputTool::new(background.clone());

        // First read eventually sees "first"; the next read must not repeat it.
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let out = tool
                    .execute(
                        TaskOutputToolParams { id: id.to_string() },
                        ToolCallContext::default(),
                    )
                    .await
                    .unwrap();
                if out.contains("first") {
                    break out;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("output never arrived");
        assert!(out.contains("status: running"), "unexpected: {out}");

        let again = tool
            .execute(
                TaskOutputToolParams { id: id.to_string() },
                ToolCallContext::default(),
            )
            .await
            .unwrap();
        assert!(
            again.contains("(no new output)"),
            "second read repeated output: {again}"
        );

        let missing = tool
            .execute(
                TaskOutputToolParams {
                    id: "bg_00000000000000000000000000000000".into(),
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap();
        assert!(
            missing.contains("Unknown or expired task id"),
            "unexpected: {missing}"
        );
        background.shutdown().await;
    }

    #[tokio::test]
    async fn task_kill_terminates_and_is_idempotent() {
        let background = Arc::new(BackgroundTasks::temporary().unwrap());
        let id = background
            .spawn(bash("sleep 39.21"), meta("victim"))
            .await
            .unwrap();
        let tool = TaskKillTool::new(background.clone());

        let out = tool
            .execute(
                TaskKillToolParams { id: id.to_string() },
                ToolCallContext::default(),
            )
            .await
            .unwrap();
        assert!(out.contains("killed"), "unexpected: {out}");

        // Idempotent: reports the settled status instead of failing.
        let again = tool
            .execute(
                TaskKillToolParams { id: id.to_string() },
                ToolCallContext::default(),
            )
            .await
            .unwrap();
        assert!(again.contains("killed"), "unexpected: {again}");

        let missing = tool
            .execute(
                TaskKillToolParams {
                    id: "bg_00000000000000000000000000000000".into(),
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap();
        assert!(
            missing.contains("Unknown or expired task id"),
            "unexpected: {missing}"
        );
        background.shutdown().await;
    }
}
