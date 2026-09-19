use std::path::Path;

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
    /// The glob pattern to match, relative to `path`. Without a `/` it matches
    /// file names at any depth, e.g. "*.rs"; with one it matches the path, e.g.
    /// "src/**/*.ts". Absolute patterns and `.` or `..` segments are rejected;
    /// set `path` to search another directory.
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
        "Find files by glob pattern using fd. A pattern without / matches file names at any depth (*.rs); a pattern with / matches paths relative to the search path (src/**/*.rs). `*` does not cross /, `**` does. A pattern with no uppercase letters ignores case. Matches hidden files. While walking directories it skips .git and anything ignored by .gitignore; a path inside .git is still searched. Returns paths relative to the search path."
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
            if params.pattern.starts_with('/') {
                return Err(ToolError::InvalidParameters(
                    "pattern must be relative; put the directory in path instead".into(),
                ));
            }
            let root = Path::new(&cwd).join(params.path.as_deref().unwrap_or("."));
            let root = tokio::fs::canonicalize(&root).await.map_err(|e| {
                ToolError::InvalidParameters(format!("cannot resolve search path: {e}"))
            })?;
            if !root.is_dir() {
                return Err(ToolError::InvalidParameters(
                    "path is not a directory".into(),
                ));
            }
            let mut pattern = params.pattern.as_str();
            while let Some(rest) = pattern.strip_prefix("./") {
                pattern = rest;
            }
            // fd only walks below the root and its paths never contain `.` or
            // `..` segments, so such a pattern could only ever match nothing.
            if pattern
                .split('/')
                .any(|segment| segment == "." || segment == "..")
            {
                return Err(ToolError::InvalidParameters(
                    "pattern must not contain . or .. segments; to search another directory, put it in path".into(),
                ));
            }

            let mut cmd = Command::new("fd");
            // Match dotfiles such as .github/ too; only .gitignore rules and
            // .git itself are skipped.
            cmd.arg("--color=never")
                .arg("--hidden")
                .arg("--exclude")
                .arg(".git");
            // fd's smart case would read the root prefix added below, so decide
            // from the caller's pattern alone.
            cmd.arg(if pattern.chars().any(char::is_uppercase) {
                "--case-sensitive"
            } else {
                "--ignore-case"
            });
            // fd matches a glob against the file name only. A pattern naming
            // directories needs the full path, anchored at the root: a bare
            // `**/` prefix would also match directories above the root, so
            // `src/**` would take in everything under a checkout in ~/src.
            // fd builds that full path from its working directory, which the
            // kernel reports canonicalized, so running in the canonical root
            // with no path argument keeps the two in step and the output
            // relative to the root.
            let pattern = if pattern.contains('/') {
                cmd.arg("--full-path");
                format!("{}/{pattern}", escape_glob(&root.to_string_lossy()))
            } else {
                pattern.to_owned()
            };
            cmd.arg("--glob").arg("--").arg(pattern);
            cmd.current_dir(&root);

            info!("Executing fd: {:?}", cmd);

            let output = run_command(cmd, ctx.clone())
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to execute fd: {e}")))?;
            match output.status {
                // fd exits 0 whether or not anything matched.
                Some(status) if status.success() => {
                    if matches!(&output.output, OutputData::Inline(text) if text.is_empty()) {
                        Ok("No matches found.".into())
                    } else {
                        Ok(output.output)
                    }
                }
                Some(status) => Err(preserve_error(
                    &ctx,
                    ToolError::ExecutionError(format!(
                        "fd failed (exit code {})",
                        status.code().unwrap_or(-1)
                    )),
                    output.output,
                )),
                None => Err(preserve_error(
                    &ctx,
                    ToolError::Aborted("Interrupted by the user before completion.".into()),
                    output.output,
                )),
            }
        }
    }
}

/// Makes a literal path safe to splice into a glob.
fn escape_glob(literal: &str) -> String {
    let mut escaped = String::with_capacity(literal.len());
    for c in literal.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
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

    #[tokio::test]
    async fn matches_hidden_files_but_not_git() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::create_dir_all(root.path().join(".github/workflows")).unwrap();
        std::fs::write(root.path().join(".git/ci.yml"), "").unwrap();
        std::fs::write(root.path().join(".github/workflows/ci.yml"), "").unwrap();
        std::fs::create_dir_all(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("target/out.yml"), "").unwrap();
        std::fs::write(root.path().join(".gitignore"), "target/\n").unwrap();
        let result = GlobTool::new(root.path().to_string_lossy().into_owned())
            .execute(
                GlobToolParams {
                    pattern: "*.yml".into(),
                    path: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap();
        let OutputData::Inline(text) = result else {
            panic!("expected inline output")
        };
        assert_eq!(text.trim(), ".github/workflows/ci.yml");
    }

    async fn glob_in(root: &Path, pattern: &str, path: Option<&str>) -> ToolResult<Vec<String>> {
        let output = GlobTool::new(root.to_string_lossy().into_owned())
            .execute(
                GlobToolParams {
                    pattern: pattern.into(),
                    path: path.map(Into::into),
                },
                ToolCallContext::default(),
            )
            .await?;
        let OutputData::Inline(text) = output else {
            panic!("expected inline output")
        };
        let mut lines: Vec<_> = text.lines().map(str::to_owned).collect();
        lines.sort();
        Ok(lines)
    }

    fn touch(root: &Path, files: &[&str]) {
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
    }

    /// The repository sits inside a directory named `src`, as a checkout in
    /// ~/src would; an unanchored `**/src/` would match that ancestor and
    /// take in every file.
    #[tokio::test]
    async fn patterns_with_directories_are_anchored_at_the_search_path() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("src").join("repo");
        touch(
            &root,
            &[
                "crates/a/Cargo.toml",
                "crates/b/Cargo.toml",
                "crates/b/nested/crates/c/Cargo.toml",
                "src/lib.rs",
                "src/deep/mod.rs",
                "tests/it.rs",
                "vendor/x/src/v.rs",
            ],
        );
        let files = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            glob_in(&root, "crates/*/Cargo.toml", None).await.unwrap(),
            files(&["crates/a/Cargo.toml", "crates/b/Cargo.toml"])
        );
        assert_eq!(
            glob_in(&root, "src/**/*.rs", None).await.unwrap(),
            files(&["src/deep/mod.rs", "src/lib.rs"])
        );
        assert_eq!(
            glob_in(&root, "./crates/a/Cargo.toml", None).await.unwrap(),
            files(&["crates/a/Cargo.toml"])
        );
        assert_eq!(
            glob_in(&root, "**/src/*.rs", None).await.unwrap(),
            files(&["src/lib.rs", "vendor/x/src/v.rs"])
        );
        // Output is relative to `path`.
        assert_eq!(
            glob_in(&root, "*/Cargo.toml", Some("crates"))
                .await
                .unwrap(),
            files(&["a/Cargo.toml", "b/Cargo.toml"])
        );
        // A bare file-name pattern still matches at any depth.
        assert_eq!(
            glob_in(&root, "v.rs", None).await.unwrap(),
            files(&["vendor/x/src/v.rs"])
        );
    }

    #[tokio::test]
    async fn glob_characters_in_the_search_path_are_literal() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("we[i]rd*{dir}");
        touch(&root, &["sub/a.rs"]);
        assert_eq!(
            glob_in(&root, "sub/*.rs", None).await.unwrap(),
            ["sub/a.rs"]
        );
    }

    /// The anchor prefix carries the root's own capitals; case handling must
    /// follow only what the caller wrote.
    #[tokio::test]
    async fn case_follows_the_callers_pattern() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("Upper");
        touch(&root, &["src/Lib.rs"]);
        assert_eq!(
            glob_in(&root, "src/lib.rs", None).await.unwrap(),
            ["src/Lib.rs"]
        );
        assert_eq!(
            glob_in(&root, "src/LIB.rs", None).await.unwrap(),
            ["No matches found."]
        );
    }

    #[tokio::test]
    async fn empty_results_and_unmatchable_patterns_are_reported() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), &["a.rs"]);
        assert_eq!(
            glob_in(root.path(), "*.zzz", None).await.unwrap(),
            ["No matches found."]
        );
        let absolute = format!("{}/*.rs", root.path().display());
        for pattern in [
            absolute.as_str(),
            "../*.rs",
            "src/../*.rs",
            "src/./a.rs",
            "..",
        ] {
            assert!(
                matches!(
                    glob_in(root.path(), pattern, None).await,
                    Err(ToolError::InvalidParameters(_))
                ),
                "{pattern}"
            );
        }
        // A leading `./` is common and harmless, so it is dropped instead.
        assert_eq!(
            glob_in(root.path(), "./a.rs", None).await.unwrap(),
            ["a.rs"]
        );
    }
}
