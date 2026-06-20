use coda_core::tool::{Tool, ToolError, ToolResult};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::debug;

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
}

impl ShellTool {
    pub fn new(cwd: String) -> Self {
        let description =
            "Execute shell commands and return stdout and stderr. You are in a Unix environment."
                .to_string();
        let schema = schemars::schema_for!(ShellToolParams);
        debug!("ShellTool schema: {:?}", schema);

        ShellTool {
            schema,
            description,
            cwd,
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
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let cwd = self.cwd.clone();
        async move {
            debug!(description = %params.description, command = %params.command, "Executing shell command");
            // `shell` is the platform-agnostic tool name; `bash` is the current backend.
            let output = Command::new("bash")
                .arg("-c")
                .arg(&params.command)
                .current_dir(&cwd)
                .output()
                .await
                .map_err(|e| {
                    ToolError::ExecutionError(format!("Failed to execute command: {}", e))
                })?;

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
