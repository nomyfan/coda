use crate::core::tool::{Tool, ToolError, ToolResult};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, info};

pub(crate) struct GlobTool {
    cwd: String,
    schema: Schema,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(crate) struct GlobToolParams {
    /// The glob pattern to match files against, e.g. "**/*.rs", "src/**/*.ts".
    pattern: String,
    /// The directory to search in. Defaults to the current working directory if not specified.
    path: Option<String>,
}

impl GlobTool {
    pub(crate) fn new(cwd: String) -> Self {
        let schema = schemars::schema_for!(GlobToolParams);
        debug!("GlobTool schema: {:?}", schema);
        GlobTool { cwd, schema }
    }
}

impl Tool for GlobTool {
    type Parameters = GlobToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern using fd. Respects .gitignore rules. Returns matching file paths."
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
            let mut cmd = Command::new("fd");
            cmd.arg("--color=never")
                .arg("--glob")
                .arg(&params.pattern);

            if let Some(ref path) = params.path {
                cmd.arg(path);
            }

            cmd.current_dir(&cwd);

            info!("Executing fd: {:?}", cmd);

            let output = cmd
                .output()
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to execute fd: {}", e)))?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            match output.status.code() {
                Some(0) => Ok(stdout.into_owned()),
                Some(1) => Ok("No matches found.".to_string()),
                _ => Err(ToolError::ExecutionError(format!(
                    "fd failed (exit code {}): {}",
                    output.status.code().unwrap_or(-1),
                    stderr
                ))),
            }
        }
    }
}
