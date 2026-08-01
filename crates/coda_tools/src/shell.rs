use std::time::Duration;

use coda_core::tool::{Tool, ToolCallContext, ToolError, ToolResult};
use schemars::{JsonSchema, Schema};
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
}

pub struct ShellTool {
    schema: Schema,
    description: String,
    cwd: String,
    timeout: Duration,
}

impl ShellTool {
    pub fn new(cwd: String) -> Self {
        let description = "Execute Bash commands and return stdout and stderr. Commands have a \
                           fixed 2-minute timeout."
            .to_string();
        let schema = schemars::schema_for!(ShellToolParams);
        debug!("ShellTool schema: {:?}", schema);

        ShellTool {
            schema,
            description,
            cwd,
            timeout: SHELL_TIMEOUT,
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
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let cwd = self.cwd.clone();
        let timeout = self.timeout;
        async move {
            debug!(description = %params.description, command = %params.command, "Executing shell command");
            // `shell` is the platform-agnostic tool name; `bash` is the current backend.
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(&params.command).current_dir(&cwd);

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
