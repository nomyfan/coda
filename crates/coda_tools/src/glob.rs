use coda_core::tool::{Tool, ToolCallContext, ToolError, ToolResult};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, info};

use crate::process::{preserve_error, run_command};
use coda_core::output::OutputData;

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
    type Output = OutputData;

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

            let output = run_command(cmd, ctx.clone())
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to execute fd: {e}")))?;
            match output.status.and_then(|status| status.code()) {
                Some(0) => Ok(output.output),
                Some(1) => Ok("No matches found.".into()),
                _ => {
                    let error = if output.status.is_none() {
                        ToolError::Aborted("Interrupted by the user before completion.".into())
                    } else {
                        ToolError::ExecutionError(format!(
                            "fd failed (exit code {})",
                            output.status.and_then(|s| s.code()).unwrap_or(-1)
                        ))
                    };
                    Err(preserve_error(&ctx, error, output.output))
                }
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
