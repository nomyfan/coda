use crate::core::tool::{Tool, ToolError, ToolResult};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::debug;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(crate) struct ShellToolParams {
    command: String,
}

pub(crate) struct ShellTool {
    schema: Schema,
    description: String,
}

impl ShellTool {
    pub(crate) fn new() -> Self {
        let description =
            "Execute shell commands and return stdout and stderr. Your are in a Unix environment."
                .to_string();
        let schema = schemars::schema_for!(ShellToolParams);
        debug!("ShellTool schema: {:?}", schema);

        ShellTool {
            schema,
            description,
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
        async move {
            let output = Command::new("sh")
                .arg("-c")
                .arg(&params.command)
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
