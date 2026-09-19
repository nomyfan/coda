use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use coda_core::llm::{FileChangeOperation, ToolArtifact};
use coda_core::tool::{Tool, ToolCallContext, ToolError, ToolResult};

use crate::locks::KeyedLock;
use crate::process::{preserve_error, run_command};
use coda_core::output::OutputData;
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::debug;

/// Largest file the fs tools will operate on. Source files, configs, and even
/// multi-megabyte lockfiles fit comfortably; anything bigger is better served
/// by grep/shell than by reading it whole into memory (and into the context).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// The key the mutating fs tools serialize on. Tool calls run concurrently —
/// several per assistant message, and in parallel across sub-agents and
/// sessions — and `edit_file` is a read-modify-write, so without exclusion one
/// of two edits to a file is silently lost. Canonicalizing makes two calls that
/// name the file by different routes collide; hard links to one inode still
/// don't (keying on `(dev, ino)` would fix that, but a file yet to be created
/// has none).
///
/// Writer-to-writer only: the lock is in-process and advisory, and edits
/// truncate in place, so a reader that skips it (`read_file`, `grep`, a
/// concurrent `shell` command, the server's knowledge poller) can still catch a
/// file mid-write. Judged not worth a temp-file-and-rename, which would cost
/// write permission on the directory and break hard links.
async fn resolve_lock_key(path: &Path) -> ToolResult<String> {
    let (parent, name) = match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => (parent, name),
        _ => {
            return Err(ToolError::InvalidParameters(
                "file_path does not name a file".to_string(),
            ));
        }
    };
    let resolved = match tokio::fs::canonicalize(path).await {
        Ok(canonical) => canonical,
        // A file yet to be created still needs a stable key; its parent, which
        // does exist, gives one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => tokio::fs::canonicalize(parent)
            .await
            .map_err(|e| {
                ToolError::ExecutionError(format!("Failed to resolve parent directory: {}", e))
            })?
            .join(name),
        Err(e) => {
            return Err(ToolError::ExecutionError(format!(
                "Failed to resolve path: {}",
                e
            )));
        }
    };
    Ok(resolved.to_string_lossy().into_owned())
}

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
    /// Maximum lines to read (default 200). A page may stop earlier to fit the output budget.
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
    type Output = OutputData;

    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read lines from a file. The file_path must be an absolute path. Returns at most 200 lines by default, each prefixed with its 1-based line number and a tab; use offset (1-based line number) and limit to read other ranges. A page may stop earlier to fit the output budget, and ends with the offset to continue from. Lines longer than 2000 bytes are cut short; use grep or shell to see the rest. Content is decoded as UTF-8, with invalid bytes replaced."
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
        async move { read_page(params, ctx).await }
    }
}

/// Longest line a page shows in full; grep or shell reach the rest of it.
const MAX_LINE_BYTES: usize = 2000;
/// Page budget held back for the continuation footer.
const FOOTER_BYTES: usize = 128;
/// Line budget held back for the line-number prefix and truncation marker.
const LINE_OVERHEAD_BYTES: usize = 128;

/// Streams the file instead of loading it, so a page costs its own size
/// however large the file is, and stops at whichever comes first: `limit`
/// lines or the output budget.
async fn read_page(params: ReadFileToolParams, ctx: ToolCallContext) -> ToolResult<OutputData> {
    use coda_core::output::{CapturePurpose, HostResultBuffer, IO_BLOCK_BYTES};
    use tokio::io::{AsyncBufReadExt, BufReader};
    let path = Path::new(&params.file_path);
    if !path.is_absolute() {
        return Err(ToolError::InvalidParameters(
            "file_path must be an absolute path".into(),
        ));
    }
    if params.offset == Some(0) || params.limit == Some(0) {
        return Err(ToolError::InvalidParameters(
            "offset and limit must be positive".into(),
        ));
    }
    let bytes = match &ctx.output_purpose {
        CapturePurpose::ModelResult | CapturePurpose::Background => ctx.output_bytes,
        CapturePurpose::Programmatic(budget) => budget.capacity() / 4,
    };
    let budget = bytes.saturating_sub(FOOTER_BYTES);
    if budget <= LINE_OVERHEAD_BYTES {
        return Err(ToolError::ResourceLimit(
            "OUTPUT_PAGE_LIMIT: no space for file content".into(),
        ));
    }
    let line_cap = MAX_LINE_BYTES.min(budget - LINE_OVERHEAD_BYTES);
    let lease = match &ctx.output_purpose {
        CapturePurpose::ModelResult | CapturePurpose::Background => None,
        CapturePurpose::Programmatic(budget) => Some(
            budget
                .reserve(bytes * 2 + IO_BLOCK_BYTES, &ctx.cancel)
                .await
                .map_err(|e| ToolError::ResourceLimit(format!("OUTPUT_LIMIT: {e:?}")))?,
        ),
    };
    let file = open_regular_file(path, false).await?;
    let mut reader = BufReader::with_capacity(IO_BLOCK_BYTES, file);
    let first = params.offset.unwrap_or(1);
    let max_lines = params.limit.unwrap_or(200);
    let mut body = String::new();
    let mut shown = 0;
    let mut line_no = 1;
    let mut raw = Vec::with_capacity(line_cap);
    let next = loop {
        let at_eof = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolError::Aborted("File read cancelled".into())),
            buf = reader.fill_buf() => buf.map_err(|e| ToolError::ExecutionError(e.to_string()))?.is_empty(),
        };
        if at_eof {
            break None;
        }
        if shown == max_lines {
            break Some((line_no, format!("reached the {max_lines}-line limit")));
        }
        // Read one line, keeping at most `line_cap` bytes of it; lines before
        // the requested offset are only scanned.
        raw.clear();
        let mut dropped = 0;
        loop {
            let buf = tokio::select! {
                _ = ctx.cancel.cancelled() => return Err(ToolError::Aborted("File read cancelled".into())),
                buf = reader.fill_buf() => buf.map_err(|e| ToolError::ExecutionError(e.to_string()))?,
            };
            if buf.is_empty() {
                break;
            }
            let newline = buf.iter().position(|byte| *byte == b'\n');
            let content = &buf[..newline.unwrap_or(buf.len())];
            let keep = if line_no >= first {
                (line_cap - raw.len()).min(content.len())
            } else {
                0
            };
            raw.extend_from_slice(&content[..keep]);
            dropped += content.len() - keep;
            let consumed = newline.map_or(buf.len(), |index| index + 1);
            reader.consume(consumed);
            if newline.is_some() {
                break;
            }
        }
        if line_no < first {
            line_no += 1;
            continue;
        }
        if dropped == 0 && raw.last() == Some(&b'\r') {
            raw.pop();
        }
        if dropped > 0 {
            // Cut at a character boundary rather than leave a split one.
            let boundary = coda_output::preview::page_boundary(&raw);
            dropped += raw.len() - boundary;
            raw.truncate(boundary);
        }
        // Cap the decoded text, not the raw bytes: each invalid byte decodes to
        // a three-byte replacement character.
        let (text, used) = coda_output::preview::decode_within(&raw, line_cap);
        dropped += raw.len() - used;
        let mut entry = format!("{line_no:>6}\t{text}");
        if dropped > 0 {
            let _ = write!(
                entry,
                " [line truncated: longer than {line_cap} bytes, {dropped} more bytes omitted]"
            );
        }
        let separator = usize::from(shown > 0);
        if body.len() + separator + entry.len() > budget {
            // `line_cap` leaves room for the prefix and marker, so a first
            // line always fits; this only guards that arithmetic.
            if shown == 0 {
                return Err(ToolError::ResourceLimit(
                    "OUTPUT_PAGE_LIMIT: a single line does not fit the output budget".into(),
                ));
            }
            break Some((line_no, format!("reached the {bytes}-byte output limit")));
        }
        if separator == 1 {
            body.push('\n');
        }
        body.push_str(&entry);
        shown += 1;
        line_no += 1;
    };
    if let Some((next, reason)) = next {
        let _ = write!(
            body,
            "\n[page truncated: {reason}; continue with offset={next}]"
        );
    }
    if lease.is_some() {
        Ok(OutputData::Buffered(HostResultBuffer { text: body, lease }))
    } else {
        Ok(OutputData::Page {
            body,
            references: vec![],
        })
    }
}

// ---- WriteFile ----

pub struct WriteFileTool {
    schema: Schema,
    locks: Arc<KeyedLock<String>>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WriteFileToolParams {
    /// The absolute path to the file to write.
    file_path: String,
    /// The content to write to the new file.
    content: String,
}

impl WriteFileTool {
    pub fn new(locks: Arc<KeyedLock<String>>) -> Self {
        let schema = schemars::schema_for!(WriteFileToolParams);
        debug!("WriteFileTool schema: {:?}", schema);
        WriteFileTool { schema, locks }
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
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let locks = self.locks.clone();
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

            // Reserve retained host memory before creating directories or the
            // file. A PTC host call therefore fails closed on budget exceed.
            ctx.record_artifact(file_diff_artifact(
                &params.file_path,
                FileChangeOperation::Create,
                "",
                &params.content,
            ))?;

            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    ToolError::ExecutionError(format!("Failed to create parent directories: {}", e))
                })?;
            }

            // `create_new` already makes creation exclusive; the lock covers the
            // gap after it, where the file exists but is still empty or half
            // written and an `edit_file` must not see it.
            let _guard = locks.lock(resolve_lock_key(path).await?).await;

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
    locks: Arc<KeyedLock<String>>,
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
    pub fn new(locks: Arc<KeyedLock<String>>) -> Self {
        let schema = schemars::schema_for!(EditFileToolParams);
        debug!("EditFileTool schema: {:?}", schema);
        EditFileTool { schema, locks }
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
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let locks = self.locks.clone();
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

            // Held across the whole read-modify-write: anything narrower lets a
            // second edit read the same original and write back a version that
            // never saw the first edit.
            let _guard = locks.lock(resolve_lock_key(path).await?).await;

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

            // The file lock is still held, but no mutation has happened yet.
            // Reserve the complete retained patch before seek/truncate/write.
            ctx.record_artifact(file_diff_artifact(
                &params.file_path,
                FileChangeOperation::Modify,
                &content,
                &updated,
            ))?;

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

fn file_diff_artifact(
    path: &str,
    operation: FileChangeOperation,
    before: &str,
    after: &str,
) -> ToolArtifact {
    let original_path = git_patch_path('a', path);
    let modified_path = git_patch_path('b', path);
    let (original_filename, modified_filename, mode) = match operation {
        FileChangeOperation::Create => {
            ("/dev/null".to_string(), modified_path.clone(), Some("new"))
        }
        FileChangeOperation::Modify => (original_path.clone(), modified_path.clone(), None),
        FileChangeOperation::Delete => (
            original_path.clone(),
            "/dev/null".to_string(),
            Some("deleted"),
        ),
    };

    let mut options = diffy::DiffOptions::new();
    options
        .set_original_filename(original_filename)
        .set_modified_filename(modified_filename);

    let mut patch = format!("diff --git {original_path} {modified_path}\n");
    if let Some(mode) = mode {
        writeln!(patch, "{mode} file mode 100644").unwrap();
    }
    patch.push_str(
        &diffy::PatchFormatter::new()
            .fmt_patch(&options.create_patch(before, after))
            .to_string(),
    );

    ToolArtifact::FileDiff {
        path: path.to_string(),
        operation,
        patch,
    }
}

fn git_patch_path(prefix: char, path: &str) -> String {
    let path = format!("{prefix}/{path}");
    if !path
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, '\\' | '"'))
    {
        return path;
    }

    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for ch in path.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            ch if ch.is_control() => {
                let mut bytes = [0; 4];
                for byte in ch.encode_utf8(&mut bytes).bytes() {
                    write!(quoted, "\\{byte:03o}").unwrap();
                }
            }
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
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
    type Output = OutputData;

    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List the contents of a directory. The path must be an absolute path. Includes hidden entries except .git and anything ignored by .gitignore; a path inside .git is still listed."
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
            // Dotfiles such as .github/ or .env.example matter to an agent, so
            // only .gitignore rules hide entries; .git itself is never useful.
            cmd.arg("--color=never")
                .arg("--hidden")
                .arg("--exclude")
                .arg(".git")
                .arg("--glob")
                .arg("*")
                .arg("--exact-depth")
                .arg("1")
                .arg(&params.path);
            let output = run_command(cmd, ctx.clone())
                .await
                .map_err(|e| ToolError::ExecutionError(e.to_string()))?;
            match output.status {
                Some(status) if status.success() => {
                    if matches!(&output.output, OutputData::Inline(text) if text.is_empty()) {
                        Ok("Directory is empty or all entries are ignored."
                            .to_string()
                            .into())
                    } else {
                        Ok(output.output)
                    }
                }
                Some(_) => Err(preserve_error(
                    &ctx,
                    ToolError::ExecutionError("fd failed".into()),
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

#[cfg(test)]
#[path = "fs_tests.rs"]
mod tests;
