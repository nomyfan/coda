//! Resource limits and metadata shared by output producers and consumers.

use std::path::PathBuf;
use std::{future::Future, pin::Pin, sync::Arc};

use serde::{Deserialize, Serialize};

pub mod budget;
pub mod config;

pub use budget::{BufferBudget, BufferLease, BufferLimitError};
pub use config::{ModelOutputLimits, OutputLimits, PtcResourceLimits, ResourceLimits};

pub const IO_BLOCK_BYTES: usize = 16 * 1024;
pub const FINALIZE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

pub type OutputFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Which execution path a capture serves. The store derives memory
/// accounting, spilling and retention from it.
#[derive(Clone, Debug)]
pub enum CapturePurpose {
    /// A tool call awaited within a turn, whose result goes back to the
    /// calling agent's model — a background subagent's own calls included.
    /// Output stays in memory while small and spills to files once it
    /// outgrows that; spilled output is sealed with an [`OutputRef`] and kept
    /// for the retention period.
    Foreground,
    /// A background task's archive, read across turns by `task_output` and
    /// the task panel. Every byte goes straight to files, sealed with an
    /// [`OutputRef`]. Only the task archive captures this way.
    Background,
    /// A host tool call from a `run_javascript` script. Capture memory is
    /// reserved from the script's budget, and the output always ends in a
    /// temporary file with no [`OutputRef`], removed once its buffer is
    /// dropped.
    Programmatic(BufferBudget),
}

/// Who consumes a tool call's result, and the budget a page of it is sized
/// from.
#[derive(Clone, Debug)]
pub enum ResultBudget {
    /// The result goes back to the calling agent's model.
    Model {
        /// Largest page the model may be handed, in bytes.
        page_bytes: usize,
    },
    /// The result goes to a `run_javascript` script; pages and buffers draw
    /// from its budget.
    Script(BufferBudget),
}

impl ResultBudget {
    /// How output captured for this result is stored.
    pub fn capture_purpose(&self) -> CapturePurpose {
        match self {
            Self::Model { .. } => CapturePurpose::Foreground,
            Self::Script(budget) => CapturePurpose::Programmatic(budget.clone()),
        }
    }

    /// Largest page a paging tool should return.
    pub fn page_bytes(&self) -> usize {
        match self {
            Self::Model { page_bytes } => *page_bytes,
            Self::Script(budget) => budget.capacity() / 4,
        }
    }

    /// Reserve memory for building a result: charged to a script's budget,
    /// free for the model, whose pages are already bounded by `page_bytes`.
    pub async fn reserve(
        &self,
        bytes: usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Option<BufferLease>, BufferLimitError> {
        match self {
            Self::Model { .. } => Ok(None),
            Self::Script(budget) => budget.reserve(bytes, cancel).await.map(Some),
        }
    }
}

/// A result's full text held in memory for a script, and the lease charging
/// it to the script's budget.
///
/// The lease stays alive through native-to-JS conversion, not just Promise enqueue.
#[derive(Debug)]
pub struct HostResultBuffer {
    /// The complete result text.
    pub text: String,
    /// Budget held for `text`; `None` leaves the caller to reserve it before
    /// delivery.
    pub lease: Option<BufferLease>,
}

pub trait OutputBuffer: Send + Sync + std::fmt::Debug {
    fn materialize<'a>(
        &'a self,
        budget: &'a BufferBudget,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> OutputFuture<'a, Result<HostResultBuffer, String>>;
}

/// A tool's result on its way out, before it is rendered for the model or
/// materialized for a script.
///
/// Captured bytes are never implicitly formatted or copied back into memory.
#[derive(Debug)]
pub enum OutputData {
    /// Text held in memory. Rendering passes it through when it fits the
    /// call's model budget, and otherwise retains it to a file and hands the
    /// model a preview and the path.
    Inline(String),
    /// Output the store kept outside memory: spilled to files, or lost to a
    /// storage failure. The model sees the preview and any file reference;
    /// the full bytes stay in the store, and `buffer` reads them back only
    /// when a script needs the whole result.
    Captured(CapturedOutput),
    /// One page a paging tool (`read_file`, `task_output`) already sized to
    /// the call's page budget, delivered verbatim: overrunning the budget is a
    /// delivery error, never a truncation.
    Page {
        /// The page text, including any continuation notes.
        body: String,
        /// Saved files the page was read from, listed for the model.
        references: Vec<OutputRef>,
    },
    /// A result already materialized for a `run_javascript` script, its
    /// memory held by a lease on the script's budget. It never reaches the
    /// model.
    Buffered(HostResultBuffer),
}

/// What a result carries for output kept outside memory: a preview for the
/// model, the saved files once sealed, any storage failure, and a handle to
/// the full bytes.
#[derive(Clone, Debug)]
pub struct CapturedOutput {
    /// A `run_javascript` report's `ok`, shown ahead of the preview; `None`
    /// for every other tool.
    pub report_ok: Option<bool>,
    /// Head and tail of each channel; rendering may shorten it further.
    pub preview: String,
    /// The saved files, once sealed. `None` for a script's temporary capture
    /// or when saving failed.
    pub reference: Option<OutputRef>,
    /// Why the saved output is incomplete or missing.
    pub failure: Option<StorageFailure>,
    /// Reads the full bytes back when a script needs them.
    pub buffer: Arc<dyn OutputBuffer>,
}

impl From<String> for OutputData {
    fn from(value: String) -> Self {
        Self::Inline(value)
    }
}

impl From<&str> for OutputData {
    fn from(value: &str) -> Self {
        Self::Inline(value.into())
    }
}

impl OutputData {
    pub fn unavailable(preview: String, failure: StorageFailure) -> Self {
        Self::Captured(CapturedOutput {
            report_ok: None,
            preview,
            reference: None,
            failure: Some(failure),
            buffer: Arc::new(UnavailableBuffer),
        })
    }

    pub fn with_prefix(self, prefix: String) -> Self {
        match self {
            Self::Buffered(mut buffer) => {
                buffer.text.insert_str(0, &prefix);
                Self::Buffered(buffer)
            }
            Self::Inline(text) => Self::Inline(format!("{prefix}{text}")),
            Self::Page { body, references } => Self::Page {
                body: format!("{prefix}{body}"),
                references,
            },
            Self::Captured(mut output) => {
                output.preview.insert_str(0, &prefix);
                output.buffer = Arc::new(PrefixedBuffer {
                    prefix,
                    inner: output.buffer,
                });
                Self::Captured(output)
            }
        }
    }

    pub async fn materialize(
        self,
        budget: &BufferBudget,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<HostResultBuffer, String> {
        match self {
            Self::Buffered(buffer) => Ok(buffer),
            Self::Inline(text) | Self::Page { body: text, .. } => {
                let bytes = text
                    .len()
                    .checked_mul(2)
                    .ok_or("OUTPUT_LIMIT: conversion size overflow")?;
                let lease = budget
                    .reserve(bytes, cancel)
                    .await
                    .map_err(|error| format!("OUTPUT_LIMIT: {error:?}"))?;
                Ok(HostResultBuffer {
                    text,
                    lease: Some(lease),
                })
            }
            Self::Captured(output) => output.buffer.materialize(budget, cancel).await,
        }
    }
}

/// Prepends `prefix` when materialized, so [`OutputData::with_prefix`] never
/// copies captured bytes.
#[derive(Debug)]
struct PrefixedBuffer {
    /// Text placed before the inner buffer's.
    prefix: String,
    /// The buffer being prefixed.
    inner: Arc<dyn OutputBuffer>,
}

impl OutputBuffer for PrefixedBuffer {
    fn materialize<'a>(
        &'a self,
        budget: &'a BufferBudget,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> OutputFuture<'a, Result<HostResultBuffer, String>> {
        Box::pin(async move {
            let mut result = self.inner.materialize(budget, cancel).await?;
            result.text.insert_str(0, &self.prefix);
            Ok(result)
        })
    }
}

pub trait OutputCapture: Send {
    fn require_file(&mut self);
    fn set_deadline(&mut self, deadline: tokio::time::Instant);
    fn reader(&self) -> Arc<dyn OutputReader>;
    fn fail(&mut self, failure: StorageFailure);
    /// The caller supplies at most IO_BLOCK_BYTES per append.
    fn append(&mut self, channel: Channel, bytes: Vec<u8>) -> OutputFuture<'_, ()>;
    fn finish(self: Box<Self>, deadline: tokio::time::Instant)
    -> OutputFuture<'static, OutputData>;
}

/// A capture's state at one moment: its preview, per-channel byte counts and,
/// once sealed, the reference or failure. Background task manifests persist
/// it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputSnapshot {
    /// A short preview across all channels, filled in when the capture
    /// finishes.
    #[serde(default)]
    pub preview: String,
    /// The capture has finished; no more bytes will arrive.
    #[serde(default)]
    pub sealed: bool,
    /// The output these counts belong to.
    pub id: OutputId,
    /// Byte counts, one entry per channel.
    pub channels: Vec<ChannelBytes>,
    /// The saved files, set when a capture that reached disk is sealed.
    pub reference: Option<OutputRef>,
    /// Why the saved output is incomplete or missing.
    pub failure: Option<StorageFailure>,
}

/// How many bytes one channel has captured, and how many of them reached disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelBytes {
    /// The channel counted.
    pub channel: Channel,
    /// Bytes the producer wrote.
    pub captured: u64,
    /// Bytes written to disk; less than `captured` once a limit or failure
    /// stops saving.
    pub saved: u64,
}

/// One `task_output` read of a background task's channel: the byte range a
/// consumer process was shown. Once its tool result is committed, it advances
/// that consumer's [`ReadProgress`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadReceipt {
    /// Process id of the agent that read.
    pub consumer: String,
    /// The task read.
    pub task: crate::task::TaskId,
    /// The channel read.
    pub channel: Channel,
    /// First byte shown.
    pub start: u64,
    /// One past the last byte shown.
    pub end: u64,
    /// Bytes the channel had captured at the time of the read.
    pub total: u64,
    /// The task had already finished when read.
    pub terminal: bool,
    /// The read left no unread bytes on any of the task's channels.
    pub complete: bool,
}

/// How far a consumer process has read one channel of a background task; its
/// next `task_output` read starts here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadProgress {
    /// Process id of the agent reading.
    pub consumer: String,
    /// The task being read.
    pub task: crate::task::TaskId,
    /// The channel being read.
    pub channel: Channel,
    /// Bytes already read; the next read starts here.
    pub offset: u64,
}

pub trait OutputReader: Send + Sync {
    fn snapshot(&self) -> OutputSnapshot;
    fn read(
        &self,
        channel: Channel,
        offset: u64,
        bytes: usize,
    ) -> OutputFuture<'_, Result<Vec<u8>, String>>;
}

pub trait OutputStore: Send + Sync {
    fn retain_source(
        &self,
        owner: OutputOwner,
        source: crate::llm::MessageId,
        text: String,
        deadline: tokio::time::Instant,
    ) -> OutputFuture<'_, OutputData>;
    fn limits(&self) -> &OutputLimits;
    fn begin(
        &self,
        owner: OutputOwner,
        channels: Vec<Channel>,
        purpose: CapturePurpose,
    ) -> OutputFuture<'_, Result<Box<dyn OutputCapture>, String>>;
    fn retain(
        &self,
        owner: OutputOwner,
        text: String,
        deadline: tokio::time::Instant,
    ) -> OutputFuture<'_, OutputData>;
}

/// Identifies one saved output and names its directory in the store. Random,
/// except for output retained for a message, where it derives from the owner
/// and message id so retaining again finds the same object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputId(
    /// Rendered in simple form as the directory name.
    uuid::Uuid,
);

impl OutputId {
    pub fn for_source(owner: &OutputOwner, source: crate::llm::MessageId) -> Self {
        let name = format!("{}\0{}\0{}", owner.workspace_id, owner.session_id, source);
        Self(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            name.as_bytes(),
        ))
    }

    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for OutputId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for OutputId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.simple().fmt(f)
    }
}

/// The session an output belongs to: quotas are charged to it and cleanup is
/// scoped by it. Empty outside a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutputOwner {
    /// The session's workspace, as configured on the server.
    pub workspace_id: String,
    /// The session within that workspace.
    pub session_id: String,
}

/// One named stream of a saved output, stored as its own file.
///
/// Each producer uses a fixed set, so only these combinations occur:
///
/// - External commands (`shell`, and `grep`/`glob`/`ls`, which run `rg`/`fd`),
///   foreground or background: `Stdout` + `Stderr`.
/// - Any other tool result, and a background subagent's answer: `Result`.
/// - `run_javascript`: `ResultJson` + `Log`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// A command's standard output.
    Stdout,
    /// A command's standard error.
    Stderr,
    /// A plain-text result: an oversized tool output or a subagent's answer.
    Result,
    /// A `run_javascript` report (`ok`, `value` or `error`), as JSON.
    ResultJson,
    /// What a `run_javascript` script printed with `console.log`.
    Log,
}

impl Channel {
    /// Most channels the store accepts for one output. The producers listed
    /// on [`Channel`] use at most two; this is the store's own bound.
    pub const MAX_PER_OUTPUT: usize = 4;

    pub const ALL: [Self; 5] = [
        Self::Stdout,
        Self::Stderr,
        Self::Result,
        Self::ResultJson,
        Self::Log,
    ];

    pub fn file_name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout.txt",
            Self::Stderr => "stderr.txt",
            Self::Result => "result.txt",
            Self::ResultJson => "result.json",
            Self::Log => "log.txt",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Result | Self::ResultJson => "result",
            Self::Log => "log",
        }
    }
}

/// Why output was not saved in full. It only affects what was kept, never
/// whether the tool itself succeeded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageFailure {
    /// The output reached the per-result size limit.
    ResultLimit,
    /// The session reached its disk quota.
    SessionQuota,
    /// The whole service reached its disk quota.
    ServiceQuota,
    /// The session or service holds too many saved outputs.
    ObjectLimit,
    /// A filesystem operation failed.
    Io,
    /// Saving did not finish before its deadline.
    FinalizeTimeout,
    /// Capture stopped before the producer finished, e.g. its pipes were
    /// abandoned or the server restarted mid-task.
    Incomplete,
}

impl std::fmt::Display for StorageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ResultLimit => "the per-result size limit was reached",
            Self::SessionQuota => "the session disk quota was reached",
            Self::ServiceQuota => "the service disk quota was reached",
            Self::ObjectLimit => "the saved output count limit was reached",
            Self::Io => "writing to disk failed",
            Self::FinalizeTimeout => "saving timed out",
            Self::Incomplete => "the capture was incomplete",
        })
    }
}

/// One channel's saved file: where it lives and how much of the captured
/// bytes it holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputChannelRef {
    /// The channel saved.
    pub channel: Channel,
    /// Where the file lives, shown to the model.
    pub path: PathBuf,
    /// Bytes the producer wrote.
    pub captured_bytes: u64,
    /// Bytes in the file; less than `captured_bytes` when saving stopped
    /// early.
    pub saved_bytes: u64,
}

/// A sealed saved output the model can read by path: one file per channel,
/// whether anything is missing, and when it expires.
///
/// Only published after the files and manifest have passed the persistence barrier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRef {
    /// The saved output.
    pub id: OutputId,
    /// One file per channel.
    pub channels: Vec<OutputChannelRef>,
    /// Every captured byte was saved.
    pub complete: bool,
    /// Why saving stopped short, when it did.
    pub failure: Option<StorageFailure>,
    /// When the files were sealed.
    pub sealed_at: jiff::Timestamp,
    /// When retention ends and the files may be deleted.
    pub expires_at: jiff::Timestamp,
}

/// Plain-text lines telling the model where retained output lives, how much of
/// it was saved and why any is missing. `failure` covers output that has no
/// reference, or a failure its references do not already report.
pub fn describe_saved(references: &[OutputRef], failure: Option<&StorageFailure>) -> String {
    let mut lines = Vec::new();
    for reference in references {
        // An empty channel still has a file, but naming it tells the model nothing.
        for channel in reference.channels.iter().filter(|c| c.captured_bytes > 0) {
            let size = if channel.saved_bytes == channel.captured_bytes {
                format!("{} bytes", channel.captured_bytes)
            } else {
                format!(
                    "{} of {} bytes",
                    channel.saved_bytes, channel.captured_bytes
                )
            };
            lines.push(format!(
                "[{} saved to {} ({size})]",
                channel.channel.name(),
                channel.path.display()
            ));
        }
        if let Some(failure) = &reference.failure {
            lines.push(format!("[saved output is incomplete: {failure}]"));
        }
        lines.push(format!(
            "[saved output expires at {}]",
            reference.expires_at
        ));
    }
    if let Some(failure) = failure
        && !references.iter().any(|r| r.failure.is_some())
    {
        lines.push(if references.is_empty() {
            format!("[full output was not saved: {failure}]")
        } else {
            format!("[saved output is incomplete: {failure}]")
        });
    }
    lines.join("\n")
}

/// A session's output setup, handed to the runtime and on to each tool call:
/// the store, the owner its output is charged to, and the PTC limits.
#[derive(Clone)]
pub struct OutputRuntime {
    /// Where the session's output is captured and saved.
    pub store: Arc<dyn OutputStore>,
    /// The session output is charged to.
    pub owner: OutputOwner,
    /// Resource limits for `run_javascript`.
    pub ptc: PtcResourceLimits,
}

/// Stands in for output whose bytes were not kept; materializing it always
/// fails.
#[derive(Debug)]
struct UnavailableBuffer;
impl OutputBuffer for UnavailableBuffer {
    fn materialize<'a>(
        &'a self,
        _: &'a BufferBudget,
        _: &'a tokio_util::sync::CancellationToken,
    ) -> OutputFuture<'a, Result<HostResultBuffer, String>> {
        Box::pin(async { Err("OUTPUT_INCOMPLETE: complete output was not retained".into()) })
    }
}
