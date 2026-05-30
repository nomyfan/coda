use std::path::Path;

use coda_core::tool::{Tool, ToolError, ToolResult};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::debug;

// ---- ReadFile ----

pub struct ReadFileTool {
    schema: Schema,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadFileToolParams {
    /// The absolute path to the file to read.
    file_path: String,
    /// The line number to start reading from (1-based). If not specified, reads from the beginning.
    offset: Option<usize>,
    /// The number of lines to read. If not specified, reads to the end of the file.
    limit: Option<usize>,
}

impl ReadFileTool {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let schema = schemars::schema_for!(ReadFileToolParams);
        debug!("ReadFileTool schema: {:?}", schema);
        ReadFileTool { schema }
    }
}

impl Tool for ReadFileTool {
    type Parameters = ReadFileToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. The file_path must be an absolute path. You can optionally specify offset (1-based line number) and limit to read a specific range of lines."
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
            let path = Path::new(&params.file_path);
            if !path.is_absolute() {
                return Err(ToolError::InvalidParameters(
                    "file_path must be an absolute path".to_string(),
                ));
            }

            let content = tokio::fs::read_to_string(path)
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to read file: {}", e)))?;

            let lines: Vec<&str> = content.lines().collect();
            let total = lines.len();

            let start = match params.offset {
                Some(offset) if offset >= 1 => offset - 1,
                Some(_) => {
                    return Err(ToolError::InvalidParameters(
                        "offset must be >= 1".to_string(),
                    ));
                }
                None => 0,
            };

            let end = match params.limit {
                Some(limit) => (start + limit).min(total),
                None => total,
            };

            if start >= total {
                return Ok(String::new());
            }

            let result: String = lines[start..end]
                .iter()
                .enumerate()
                .map(|(i, line)| format!("{:>6}\t{}", start + i + 1, line))
                .collect::<Vec<_>>()
                .join("\n");

            Ok(result)
        }
    }
}

// ---- WriteFile ----

pub struct WriteFileTool {
    schema: Schema,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WriteFileToolParams {
    /// The absolute path to the file to write.
    file_path: String,
    /// The content to write to the file. This will overwrite the existing file content.
    content: String,
}

impl WriteFileTool {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let schema = schemars::schema_for!(WriteFileToolParams);
        debug!("WriteFileTool schema: {:?}", schema);
        WriteFileTool { schema }
    }
}

impl Tool for WriteFileTool {
    type Parameters = WriteFileToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file. The file_path must be an absolute path. If the file exists, it will be overwritten. Parent directories will be created if they don't exist."
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
            let path = Path::new(&params.file_path);
            if !path.is_absolute() {
                return Err(ToolError::InvalidParameters(
                    "file_path must be an absolute path".to_string(),
                ));
            }

            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    ToolError::ExecutionError(format!("Failed to create parent directories: {}", e))
                })?;
            }

            let bytes = params.content.len();
            tokio::fs::write(path, &params.content)
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to write file: {}", e)))?;

            Ok(format!(
                "Successfully wrote {} bytes to {}",
                bytes, params.file_path
            ))
        }
    }
}

// ---- EditFile ----

pub struct EditFileTool {
    schema: Schema,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct EditFileToolParams {
    /// The absolute path to the file to edit.
    file_path: String,
    /// The exact text to replace. Must match the file content exactly, including
    /// indentation and whitespace. Do NOT include the line-number prefix produced
    /// by `read_file`. Unless `replace_all` is true, this text must be unique in
    /// the file.
    old_string: String,
    /// The text to replace `old_string` with.
    new_string: String,
    /// Replace every occurrence of `old_string` instead of requiring a unique
    /// match. Defaults to false.
    replace_all: Option<bool>,
}

impl EditFileTool {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let schema = schemars::schema_for!(EditFileToolParams);
        debug!("EditFileTool schema: {:?}", schema);
        EditFileTool { schema }
    }
}

impl Tool for EditFileTool {
    type Parameters = EditFileToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit an existing file by replacing an exact string. The file_path must be an absolute path. `old_string` must match the file content exactly (including whitespace and indentation) and must NOT include the line-number prefix from read_file. Unless `replace_all` is true, `old_string` must appear exactly once. To create a new file use write_file instead."
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
            let path = Path::new(&params.file_path);
            if !path.is_absolute() {
                return Err(ToolError::InvalidParameters(
                    "file_path must be an absolute path".to_string(),
                ));
            }

            if params.old_string.is_empty() {
                return Err(ToolError::InvalidParameters(
                    "old_string must not be empty".to_string(),
                ));
            }

            if params.old_string == params.new_string {
                return Err(ToolError::InvalidParameters(
                    "old_string and new_string are identical; nothing to change".to_string(),
                ));
            }

            let content = tokio::fs::read_to_string(path)
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to read file: {}", e)))?;

            let matches = content.matches(&params.old_string).count();
            if matches == 0 {
                return Err(ToolError::InvalidParameters(
                    "old_string not found in file".to_string(),
                ));
            }

            let replace_all = params.replace_all.unwrap_or(false);
            let (updated, replaced) = if replace_all {
                (
                    content.replace(&params.old_string, &params.new_string),
                    matches,
                )
            } else {
                if matches > 1 {
                    return Err(ToolError::InvalidParameters(format!(
                        "old_string is not unique ({} matches); add more surrounding context to make it unique, or pass replace_all",
                        matches
                    )));
                }
                (
                    content.replacen(&params.old_string, &params.new_string, 1),
                    1,
                )
            };

            tokio::fs::write(path, &updated)
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to write file: {}", e)))?;

            Ok(format!(
                "Successfully replaced {} occurrence(s) in {}",
                replaced, params.file_path
            ))
        }
    }
}

// ---- ListDirectory ----

pub struct ListDirectoryTool {
    schema: Schema,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListDirectoryToolParams {
    /// The absolute path to the directory to list.
    path: String,
}

impl ListDirectoryTool {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let schema = schemars::schema_for!(ListDirectoryToolParams);
        debug!("ListDirectoryTool schema: {:?}", schema);
        ListDirectoryTool { schema }
    }
}

impl Tool for ListDirectoryTool {
    type Parameters = ListDirectoryToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List the contents of a directory. The path must be an absolute path. Respects .gitignore rules."
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
            let path = Path::new(&params.path);
            if !path.is_absolute() {
                return Err(ToolError::InvalidParameters(
                    "path must be an absolute path".to_string(),
                ));
            }

            let output = Command::new("fd")
                .arg("--color=never")
                .arg("--glob")
                .arg("*")
                .arg("--exact-depth")
                .arg("1")
                .arg(&params.path)
                .output()
                .await
                .map_err(|e| ToolError::ExecutionError(e.to_string()))?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            match output.status.code() {
                Some(0) if stdout.is_empty() => {
                    Ok("Directory is empty or all entries are ignored.".to_string())
                }
                Some(0) => Ok(stdout.into_owned()),
                _ => Err(ToolError::ExecutionError(stderr.into_owned())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(name: &str, content: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("coda_edit_test_{}_{}", std::process::id(), name));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[tokio::test]
    async fn edit_replaces_unique_match() {
        let path = tmp_file("unique", "hello world\nfoo bar\n");
        let tool = EditFileTool::new();
        let result = tool
            .execute(EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "foo bar".to_string(),
                new_string: "baz qux".to_string(),
                replace_all: None,
            })
            .await
            .unwrap();
        assert!(result.contains("1 occurrence"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "hello world\nbaz qux\n"
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn edit_errors_when_not_found() {
        let path = tmp_file("notfound", "hello world\n");
        let tool = EditFileTool::new();
        let err = tool
            .execute(EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "missing".to_string(),
                new_string: "x".to_string(),
                replace_all: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn edit_errors_on_ambiguous_match() {
        let path = tmp_file("ambiguous", "x\nx\n");
        let tool = EditFileTool::new();
        let err = tool
            .execute(EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "x".to_string(),
                new_string: "y".to_string(),
                replace_all: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn edit_replace_all() {
        let path = tmp_file("all", "x\nx\nx\n");
        let tool = EditFileTool::new();
        let result = tool
            .execute(EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "x".to_string(),
                new_string: "y".to_string(),
                replace_all: Some(true),
            })
            .await
            .unwrap();
        assert!(result.contains("3 occurrence"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\ny\ny\n");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn edit_errors_on_identical_strings() {
        let path = tmp_file("identical", "x\n");
        let tool = EditFileTool::new();
        let err = tool
            .execute(EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "x".to_string(),
                new_string: "x".to_string(),
                replace_all: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn edit_requires_absolute_path() {
        let tool = EditFileTool::new();
        let err = tool
            .execute(EditFileToolParams {
                file_path: "relative.txt".to_string(),
                old_string: "a".to_string(),
                new_string: "b".to_string(),
                replace_all: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
    }

    #[tokio::test]
    async fn edit_errors_on_empty_old_string() {
        let path = tmp_file("empty_old", "hello\n");
        let tool = EditFileTool::new();
        let err = tool
            .execute(EditFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                old_string: "".to_string(),
                new_string: "x".to_string(),
                replace_all: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        std::fs::remove_file(&path).ok();
    }
}
