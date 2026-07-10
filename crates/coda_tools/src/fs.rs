use std::path::Path;

use coda_core::tool::{Tool, ToolCallContext, ToolError, ToolResult};

use crate::process::{CommandRun, run_command};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::debug;

/// Largest file the fs tools will operate on. Source files, configs, and even
/// multi-megabyte lockfiles fit comfortably; anything bigger is better served
/// by grep/shell than by reading it whole into memory (and into the context).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Open `path` and verify, from the opened handle, that it is a regular file
/// — not a symlink (of the final path component), directory, or other special
/// file. Callers must do all IO through the returned handle: re-opening by
/// path would let a concurrent swap of the path redirect the IO. O_NONBLOCK
/// only stops the open itself from hanging on a FIFO; it has no effect on
/// regular-file IO.
async fn open_regular_file(path: &Path, write: bool) -> ToolResult<tokio::fs::File> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .await
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                ToolError::InvalidParameters(
                    "path is a symlink; only regular files are supported".to_string(),
                )
            } else {
                ToolError::ExecutionError(format!("Failed to open file: {}", e))
            }
        })?;
    let metadata = file
        .metadata()
        .await
        .map_err(|e| ToolError::ExecutionError(format!("Failed to stat file: {}", e)))?;
    if !metadata.is_file() {
        return Err(ToolError::InvalidParameters(
            "path is not a regular file".to_string(),
        ));
    }
    Ok(file)
}

/// Read the whole file through the handle, enforcing MAX_FILE_SIZE at read
/// time: a size probe at open would miss a file that grows while being read.
async fn read_capped(file: &mut tokio::fs::File) -> ToolResult<Vec<u8>> {
    let mut buf = Vec::new();
    file.take(MAX_FILE_SIZE + 1)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| ToolError::ExecutionError(format!("Failed to read file: {}", e)))?;
    if buf.len() as u64 > MAX_FILE_SIZE {
        return Err(ToolError::InvalidParameters(format!(
            "file is larger than the {} MiB limit",
            MAX_FILE_SIZE / (1024 * 1024),
        )));
    }
    Ok(buf)
}

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
        "Read the contents of a file. The file_path must be an absolute path. You can optionally specify offset (1-based line number) and limit to read a specific range of lines. Content is decoded as UTF-8."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        _ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        async move {
            let path = Path::new(&params.file_path);
            if !path.is_absolute() {
                return Err(ToolError::InvalidParameters(
                    "file_path must be an absolute path".to_string(),
                ));
            }

            let mut file = open_regular_file(path, false).await?;
            let buf = read_capped(&mut file).await?;
            let content = String::from_utf8_lossy(&buf);

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
    /// The content to write to the new file.
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
        "Create a new file with the given content. The file_path must be an absolute path and must not already exist — use edit_file to modify an existing file. Parent directories will be created if they don't exist."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        _ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        async move {
            let path = Path::new(&params.file_path);
            if !path.is_absolute() {
                return Err(ToolError::InvalidParameters(
                    "file_path must be an absolute path".to_string(),
                ));
            }

            // Keep the module's invariant: never create a file that read_file
            // and edit_file would then refuse to touch.
            if params.content.len() as u64 > MAX_FILE_SIZE {
                return Err(ToolError::InvalidParameters(format!(
                    "content is {} bytes, larger than the {} MiB limit",
                    params.content.len(),
                    MAX_FILE_SIZE / (1024 * 1024),
                )));
            }

            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    ToolError::ExecutionError(format!("Failed to create parent directories: {}", e))
                })?;
            }

            // O_CREAT|O_EXCL fails atomically on any existing path, including
            // symlinks (even dangling ones), closing the check-then-write race.
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .await
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        ToolError::InvalidParameters(
                            "file_path already exists; use edit_file to modify an existing file"
                                .to_string(),
                        )
                    } else {
                        ToolError::ExecutionError(format!("Failed to create file: {}", e))
                    }
                })?;

            let bytes = params.content.len();
            file.write_all(params.content.as_bytes())
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to write file: {}", e)))?;
            file.flush()
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
        "Edit an existing file by replacing an exact string. The file_path must be an absolute path and the file must be UTF-8 text. `old_string` must match the file content exactly (including whitespace and indentation) and must NOT include the line-number prefix from read_file. Unless `replace_all` is true, `old_string` must appear exactly once. To create a new file use write_file instead."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        _ctx: ToolCallContext,
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

            let mut file = open_regular_file(path, true).await?;
            let buf = read_capped(&mut file).await?;
            // A lossy decode would silently corrupt the file on write-back
            // (invalid bytes replaced with U+FFFD), so editing demands valid
            // UTF-8. read_file, which never writes back, decodes lossily.
            let content = String::from_utf8(buf).map_err(|_| {
                ToolError::InvalidParameters(
                    "file is not valid UTF-8 text; only UTF-8 text files can be edited".to_string(),
                )
            })?;

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

            file.seek(std::io::SeekFrom::Start(0))
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to write file: {}", e)))?;
            file.set_len(0)
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to write file: {}", e)))?;
            file.write_all(updated.as_bytes())
                .await
                .map_err(|e| ToolError::ExecutionError(format!("Failed to write file: {}", e)))?;
            file.flush()
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
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        async move {
            let path = Path::new(&params.path);
            if !path.is_absolute() {
                return Err(ToolError::InvalidParameters(
                    "path must be an absolute path".to_string(),
                ));
            }

            let mut cmd = Command::new("fd");
            cmd.arg("--color=never")
                .arg("--glob")
                .arg("*")
                .arg("--exact-depth")
                .arg("1")
                .arg(&params.path);
            let output = match run_command(cmd, ctx.cancel)
                .await
                .map_err(|e| ToolError::ExecutionError(e.to_string()))?
            {
                CommandRun::Completed(output) => output,
                CommandRun::Cancelled { .. } => {
                    return Err(ToolError::Aborted(
                        "Interrupted by the user before completion.".to_string(),
                    ));
                }
            };

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

    /// The cancellation context must reach the child-process runner: a token
    /// cancelled up front settles as Aborted instead of running fd.
    #[tokio::test]
    async fn ls_pre_cancelled_context_aborts() {
        let ctx = ToolCallContext::default();
        ctx.cancel.cancel();
        let result = ListDirectoryTool::new()
            .execute(
                ListDirectoryToolParams {
                    path: std::env::temp_dir().to_string_lossy().into_owned(),
                },
                ctx,
            )
            .await;
        assert!(matches!(result, Err(ToolError::Aborted(_))));
    }

    #[tokio::test]
    async fn edit_replaces_unique_match() {
        let path = tmp_file("unique", "hello world\nfoo bar\n");
        let tool = EditFileTool::new();
        let result = tool
            .execute(
                EditFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    old_string: "foo bar".to_string(),
                    new_string: "baz qux".to_string(),
                    replace_all: None,
                },
                ToolCallContext::default(),
            )
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
            .execute(
                EditFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    old_string: "missing".to_string(),
                    new_string: "x".to_string(),
                    replace_all: None,
                },
                ToolCallContext::default(),
            )
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
            .execute(
                EditFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    old_string: "x".to_string(),
                    new_string: "y".to_string(),
                    replace_all: None,
                },
                ToolCallContext::default(),
            )
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
            .execute(
                EditFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    old_string: "x".to_string(),
                    new_string: "y".to_string(),
                    replace_all: Some(true),
                },
                ToolCallContext::default(),
            )
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
            .execute(
                EditFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    old_string: "x".to_string(),
                    new_string: "x".to_string(),
                    replace_all: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn edit_requires_absolute_path() {
        let tool = EditFileTool::new();
        let err = tool
            .execute(
                EditFileToolParams {
                    file_path: "relative.txt".to_string(),
                    old_string: "a".to_string(),
                    new_string: "b".to_string(),
                    replace_all: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
    }

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("coda_fs_test_{}_{}", std::process::id(), name));
        path
    }

    fn tmp_huge_file(name: &str) -> std::path::PathBuf {
        let path = tmp_path(name);
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_FILE_SIZE + 1).unwrap();
        path
    }

    #[tokio::test]
    async fn write_refuses_huge_file() {
        let path = tmp_path("huge_write");
        std::fs::remove_file(&path).ok();
        let tool = WriteFileTool::new();
        let err = tool
            .execute(
                WriteFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    content: "a".repeat((MAX_FILE_SIZE + 1) as usize),
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn read_refuses_huge_file() {
        let path = tmp_huge_file("huge_read");
        let tool = ReadFileTool::new();
        let err = tool
            .execute(
                ReadFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    offset: None,
                    limit: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn edit_refuses_huge_file() {
        let path = tmp_huge_file("huge_edit");
        let tool = EditFileTool::new();
        let err = tool
            .execute(
                EditFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    old_string: "a".to_string(),
                    new_string: "b".to_string(),
                    replace_all: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn read_decodes_invalid_utf8_lossily() {
        let path = tmp_path("lossy_read");
        std::fs::write(&path, b"before \xFF\xFE after\n").unwrap();
        let tool = ReadFileTool::new();
        let result = tool
            .execute(
                ReadFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    offset: None,
                    limit: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap();
        assert!(result.contains("before \u{FFFD}\u{FFFD} after"));
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn edit_refuses_invalid_utf8() {
        let path = tmp_path("non_utf8_edit");
        std::fs::write(&path, b"before \xFF\xFE after\n").unwrap();
        let tool = EditFileTool::new();
        let err = tool
            .execute(
                EditFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    old_string: "before".to_string(),
                    new_string: "changed".to_string(),
                    replace_all: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        assert_eq!(std::fs::read(&path).unwrap(), b"before \xFF\xFE after\n");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn write_creates_new_file() {
        let path = tmp_path("write_new");
        std::fs::remove_file(&path).ok();
        let tool = WriteFileTool::new();
        tool.execute(
            WriteFileToolParams {
                file_path: path.to_str().unwrap().to_string(),
                content: "hello".to_string(),
            },
            ToolCallContext::default(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn write_refuses_existing_file() {
        let path = tmp_file("write_existing", "original\n");
        let tool = WriteFileTool::new();
        let err = tool
            .execute(
                WriteFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    content: "clobbered".to_string(),
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn write_refuses_existing_symlink() {
        let target = tmp_file("symlink_write_target", "content\n");
        let link = tmp_path("symlink_write_link");
        std::fs::remove_file(&link).ok();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let tool = WriteFileTool::new();
        let err = tool
            .execute(
                WriteFileToolParams {
                    file_path: link.to_str().unwrap().to_string(),
                    content: "clobbered".to_string(),
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "content\n");
        std::fs::remove_file(&link).ok();
        std::fs::remove_file(&target).ok();
    }

    #[tokio::test]
    async fn read_refuses_symlink() {
        let target = tmp_file("symlink_read_target", "content\n");
        let link = tmp_path("symlink_read_link");
        std::fs::remove_file(&link).ok();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let tool = ReadFileTool::new();
        let err = tool
            .execute(
                ReadFileToolParams {
                    file_path: link.to_str().unwrap().to_string(),
                    offset: None,
                    limit: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        std::fs::remove_file(&link).ok();
        std::fs::remove_file(&target).ok();
    }

    #[tokio::test]
    async fn read_refuses_directory() {
        let tool = ReadFileTool::new();
        let err = tool
            .execute(
                ReadFileToolParams {
                    file_path: std::env::temp_dir().to_str().unwrap().to_string(),
                    offset: None,
                    limit: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
    }

    #[tokio::test]
    async fn edit_refuses_symlink() {
        let target = tmp_file("symlink_edit_target", "content\n");
        let link = tmp_path("symlink_edit_link");
        std::fs::remove_file(&link).ok();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let tool = EditFileTool::new();
        let err = tool
            .execute(
                EditFileToolParams {
                    file_path: link.to_str().unwrap().to_string(),
                    old_string: "content".to_string(),
                    new_string: "changed".to_string(),
                    replace_all: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "content\n");
        std::fs::remove_file(&link).ok();
        std::fs::remove_file(&target).ok();
    }

    #[tokio::test]
    async fn edit_errors_on_empty_old_string() {
        let path = tmp_file("empty_old", "hello\n");
        let tool = EditFileTool::new();
        let err = tool
            .execute(
                EditFileToolParams {
                    file_path: path.to_str().unwrap().to_string(),
                    old_string: "".to_string(),
                    new_string: "x".to_string(),
                    replace_all: None,
                },
                ToolCallContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParameters(_)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        std::fs::remove_file(&path).ok();
    }
}
