use coda_core::tool::{Tool, ToolCallContext, ToolError, ToolResult};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, info};

use crate::process::{preserve_error, run_command};
use coda_core::output::OutputData;

pub struct GrepTool {
    /// Absolute path to the directory where the grep command should be executed.
    cwd: String,
    /// JSON schema for the parameters of the grep tool.
    schema: Schema,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GrepToolParams {
    /// The regex pattern to search for.
    pattern: String,
    /// The directory or file path to search in. Defaults to the current working directory if not specified.
    path: Option<String>,
    /// Optional glob pattern to filter files, e.g. "*.rs".
    glob: Option<String>,
}

impl GrepTool {
    pub fn new(cwd: String) -> Self {
        let schema = schemars::schema_for!(GrepToolParams);
        debug!("GrepTool schema: {:?}", schema);
        GrepTool { cwd, schema }
    }
}

impl Tool for GrepTool {
    type Parameters = GrepToolParams;

    type Output = OutputData;

    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents using ripgrep. Searches hidden files. While walking directories it skips .git and anything ignored by .gitignore, but a glob that matches an ignored directory's name (such as *) searches inside it. Returns matching lines with file paths and line numbers."
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
            let mut cmd = Command::new("rg");
            // Search dotfiles such as .github/ too; only .gitignore rules and
            // .git itself are skipped.
            cmd.arg("--color=never")
                .arg("--line-number")
                .arg("--hidden")
                .arg(&params.pattern)
                .arg(match &params.path {
                    Some(path) => path,
                    None => ".",
                });

            if let Some(ref glob) = params.glob {
                cmd.arg("--glob").arg(glob);
            }
            // The last matching glob wins, so this must follow the caller's
            // or a pattern like `*` would bring .git back.
            cmd.arg("--glob").arg("!.git");

            cmd.current_dir(&cwd);

            info!("Executing rg: {:?}", cmd);

            let output = run_command(cmd, ctx.clone())
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to execute rg: {e}")))?;
            match output.status.and_then(|status| status.code()) {
                Some(0) => Ok(output.output),
                Some(1) => Ok("No matches found.".into()),
                _ => {
                    let error = if output.status.is_none() {
                        ToolError::Aborted("Interrupted by the user before completion.".into())
                    } else {
                        ToolError::ExecutionError(format!(
                            "rg failed (exit code {})",
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
    /// cancelled up front settles as Aborted instead of running rg.
    #[tokio::test]
    async fn pre_cancelled_context_aborts() {
        let ctx = ToolCallContext::default();
        ctx.cancel.cancel();
        let result = GrepTool::new(".".into())
            .execute(
                GrepToolParams {
                    pattern: "x".into(),
                    path: None,
                    glob: None,
                },
                ctx,
            )
            .await;
        assert!(matches!(result, Err(ToolError::Aborted(_))));
    }

    #[tokio::test]
    async fn searches_hidden_files_but_not_git() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::create_dir_all(root.path().join(".github/workflows")).unwrap();
        std::fs::write(root.path().join(".git/config"), "needle\n").unwrap();
        std::fs::write(root.path().join(".github/workflows/ci.yml"), "needle\n").unwrap();
        std::fs::write(root.path().join("visible.txt"), "needle\n").unwrap();
        std::fs::create_dir_all(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("target/out.yml"), "needle\n").unwrap();
        std::fs::write(root.path().join(".gitignore"), "target/\n").unwrap();
        // `*` also matches the directory name `.git`; the exclusion must
        // still win over it.
        for glob in [None, Some("*.yml".to_owned()), Some("*".to_owned())] {
            let result = GrepTool::new(root.path().to_string_lossy().into_owned())
                .execute(
                    GrepToolParams {
                        pattern: "needle".into(),
                        path: None,
                        glob: glob.clone(),
                    },
                    ToolCallContext::default(),
                )
                .await
                .unwrap();
            let OutputData::Inline(text) = result else {
                panic!("expected inline output")
            };
            assert!(text.contains(".github/workflows/ci.yml"), "{text}");
            assert!(!text.contains(".git/config"), "{text}");
            assert_eq!(
                text.contains("visible.txt"),
                glob.as_deref() != Some("*.yml"),
                "{text}"
            );
            if glob.as_deref() != Some("*") {
                assert!(!text.contains("target/out.yml"), "{text}");
            }
        }
    }
}
