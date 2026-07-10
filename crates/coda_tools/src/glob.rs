use coda_core::tool::{Tool, ToolCallContext, ToolError, ToolResult};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, info};

use crate::process::{CommandOutcome, run_command};

pub struct GlobTool {
    cwd: String,
    schema: Schema,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GlobToolParams {
    /// The glob pattern to match files against, e.g. "**/*.rs", "src/**/*.ts".
    pattern: String,
    /// The directory to search in. Defaults to the current working directory if not specified.
    path: Option<String>,
}

impl GlobTool {
    pub fn new(cwd: String) -> Self {
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
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let cwd = self.cwd.clone();

        async move {
            let mut cmd = Command::new("fd");
            cmd.arg("--color=never").arg("--glob").arg(&params.pattern);

            if let Some(ref path) = params.path {
                cmd.arg(path);
            }

            cmd.current_dir(&cwd);

            info!("Executing fd: {:?}", cmd);

            let output = match run_command(cmd, ctx.cancel)
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to execute fd: {}", e)))?
            {
                CommandOutcome::Completed(output) => output,
                CommandOutcome::Cancelled { .. } => {
                    return Err(ToolError::Aborted(
                        "Interrupted by the user before completion.".to_string(),
                    ));
                }
            };

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The cancellation context must reach the child-process runner: a token
    /// cancelled up front settles as Aborted instead of running fd.
    #[tokio::test]
    async fn pre_cancelled_context_aborts() {
        let ctx = ToolCallContext::default();
        ctx.cancel.cancel();
        let result = GlobTool::new(".".into())
            .execute(
                GlobToolParams {
                    pattern: "*.rs".into(),
                    path: None,
                },
                ctx,
            )
            .await;
        assert!(matches!(result, Err(ToolError::Aborted(_))));
    }
}
