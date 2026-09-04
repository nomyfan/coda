use std::sync::Arc;
use std::time::Duration;

use coda_core::tool::{Tool, ToolCallContext, ToolError, ToolResult};
use coda_process::{BackgroundProcesses, TaskMeta};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::debug;

use crate::process::{CommandOutcome, run_command};

const SHELL_TIMEOUT: Duration = Duration::from_secs(2 * 60);

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ShellToolParams {
    /// The shell command to execute.
    command: String,
    /// A short (5-10 word) description of what this command does, in active
    /// voice. For example: "List files in the current directory".
    description: String,
    /// Run the command as a background task: the call returns immediately
    /// with a task id, and the command is not subject to the 2-minute
    /// timeout. Use task_output to read its output incrementally and
    /// task_kill to terminate it. Use this for long-running commands (dev
    /// servers, watchers, long builds).
    #[serde(default)]
    run_in_background: Option<bool>,
}

pub struct ShellTool {
    schema: serde_json::Value,
    description: String,
    cwd: String,
    agent_name: String,
    timeout: Duration,
    /// `None` when the session has no registry: `run_in_background` is then
    /// absent from the schema, and ignored if the model invents it anyway.
    background: Option<Arc<BackgroundProcesses>>,
}

impl ShellTool {
    pub fn new(
        cwd: String,
        agent_name: String,
        background: Option<Arc<BackgroundProcesses>>,
    ) -> Self {
        // Backgrounding is how a command escapes the timeout, so the two are
        // described together — but only to an agent that can actually follow
        // a task up.
        let description = if background.is_some() {
            "Execute Bash commands and return stdout and stderr. Commands have a \
             fixed 2-minute timeout; run anything that may outlast it as a \
             background task instead of splitting or truncating it."
        } else {
            "Execute Bash commands and return stdout and stderr. Commands have a \
             fixed 2-minute timeout."
        }
        .to_string();

        let mut schema = serde_json::to_value(schemars::schema_for!(ShellToolParams))
            .expect("shell schema serializes");
        if background.is_none()
            && let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut())
        {
            props.remove("run_in_background");
        }
        debug!("ShellTool schema: {:?}", schema);

        ShellTool {
            schema,
            description,
            cwd,
            agent_name,
            timeout: SHELL_TIMEOUT,
            background,
        }
    }
}

impl Tool for ShellTool {
    type Parameters = ShellToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        &self.schema
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let cwd = self.cwd.clone();
        let agent_name = self.agent_name.clone();
        let timeout = self.timeout;
        // An invented flag must not start a task nothing can observe or kill.
        let background = params
            .run_in_background
            .unwrap_or(false)
            .then(|| self.background.clone())
            .flatten();
        async move {
            debug!(description = %params.description, command = %params.command, "Executing shell command");
            // `shell` is the platform-agnostic tool name; `bash` is the current backend.
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(&params.command).current_dir(&cwd);

            if let Some(background) = background {
                // The call settles now; the task lives outside tool-call
                // semantics (only task_kill / registry shutdown end it), and
                // outside the timeout — escaping it is the point. An already
                // aborted turn must not start work, mirroring the foreground
                // pre-cancellation check inside `run_command`.
                if ctx.cancel.is_cancelled() {
                    return Err(ToolError::Aborted(
                        "Command was aborted by the user before it started.".into(),
                    ));
                }
                let id = background
                    .spawn(
                        cmd,
                        TaskMeta {
                            command: params.command.clone(),
                            description: params.description.clone(),
                            agent_name,
                        },
                    )
                    .await
                    .map_err(|e| {
                        ToolError::ExecutionError(format!("Failed to start background task: {e}"))
                    })?;
                return Ok(format!(
                    "Started background task {id}. Use task_output to read its \
                     output and task_kill to terminate it. You will be notified \
                     when it finishes."
                ));
            }

            let execution_cancel = ctx.cancel.child_token();
            let mut command = Box::pin(run_command(cmd, execution_cancel.clone()));
            let run = match tokio::time::timeout(timeout, &mut command).await {
                Ok(run) => run,
                Err(_) => {
                    execution_cancel.cancel();
                    let _ = command.await;
                    return Err(ToolError::ExecutionError(String::from(
                        "Command timed out after the 2-minute execution limit.",
                    )));
                }
            }
            .map_err(|e| ToolError::ExecutionError(format!("Failed to execute command: {}", e)))?;

            let output = match run {
                CommandOutcome::Cancelled { stdout, stderr } => {
                    let stdout = String::from_utf8_lossy(&stdout);
                    let stderr = String::from_utf8_lossy(&stderr);
                    let mut reason =
                        String::from("Command was aborted by the user before completion.");
                    if !stdout.is_empty() {
                        reason.push_str(&format!("\nstdout (partial): {}", stdout));
                    }
                    if !stderr.is_empty() {
                        reason.push_str(&format!("\nstderr (partial): {}", stderr));
                    }
                    return Err(ToolError::Aborted(reason));
                }
                CommandOutcome::Completed(output) => output,
            };

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            if output.status.success() {
                Ok(format!("{}", stdout))
            } else {
                Ok(format!(
                    "exit code: {}\nstdout: {}\nstderr: {}",
                    output.status.code().unwrap_or(-1),
                    stdout,
                    stderr
                ))
            }
        }
    }
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
