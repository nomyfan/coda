use crate::core::tool::{Tool, ToolError, ToolResult};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, info};

pub(crate) struct GrepTool {
    /// Absolute path to the directory where the grep command should be executed.
    cwd: String,
    /// JSON schema for the parameters of the grep tool.
    schema: Schema,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(crate) struct GrepToolParams {
    /// The regex pattern to search for.
    pattern: String,
    /// The directory or file path to search in. Defaults to the current working directory if not specified.
    path: Option<String>,
    /// Optional glob pattern to filter files, e.g. "*.rs".
    glob: Option<String>,
}

impl GrepTool {
    pub(crate) fn new(cwd: String) -> Self {
        let schema = schemars::schema_for!(GrepToolParams);
        debug!("GrepTool schema: {:?}", schema);
        GrepTool { cwd, schema }
    }
}

impl Tool for GrepTool {
    type Parameters = GrepToolParams;

    type Output = String;

    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents using ripgrep. Returns matching lines with file paths and line numbers."
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
            // TODO: optimize the case where result is too large
            let mut cmd = Command::new("rg");
            cmd.arg("--color=never")
                .arg("--line-number")
                .arg(&params.pattern)
                .arg(match &params.path {
                    Some(path) => path,
                    None => ".",
                });

            if let Some(ref glob) = params.glob {
                cmd.arg("--glob").arg(glob);
            }

            cmd.current_dir(&cwd);

            info!("Executing rg: {:?}", cmd);

            let output = cmd
                .output()
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to execute rg: {}", e)))?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            match output.status.code() {
                Some(0) => Ok(stdout.into_owned()),
                Some(1) => Ok("No matches found.".to_string()),
                _ => Err(ToolError::ExecutionError(format!(
                    "rg failed (exit code {}): {}",
                    output.status.code().unwrap_or(-1),
                    stderr
                ))),
            }
        }
    }
}
