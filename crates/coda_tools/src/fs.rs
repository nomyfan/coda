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
