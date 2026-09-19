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

#[derive(Clone, Debug)]
pub enum CapturePurpose {
    ModelResult,
    Background,
    Programmatic(BufferBudget),
}

/// The lease stays alive through native-to-JS conversion, not just Promise enqueue.
#[derive(Debug)]
pub struct HostResultBuffer {
    pub text: String,
    pub lease: Option<BufferLease>,
}

pub trait OutputBuffer: Send + Sync + std::fmt::Debug {
    fn materialize<'a>(
        &'a self,
        budget: &'a BufferBudget,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> OutputFuture<'a, Result<HostResultBuffer, String>>;
}

/// Captured bytes are never implicitly formatted or copied back into memory.
#[derive(Debug)]
pub enum OutputData {
    Inline(String),
    Captured(CapturedOutput),
    Page {
        body: String,
        references: Vec<OutputRef>,
    },
    Buffered(HostResultBuffer),
}

#[derive(Clone, Debug)]
pub struct CapturedOutput {
    pub report_ok: Option<bool>,
    pub preview: String,
    pub reference: Option<OutputRef>,
    pub failure: Option<StorageFailure>,
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

#[derive(Debug)]
struct PrefixedBuffer {
    prefix: String,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputSnapshot {
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub sealed: bool,
    pub id: OutputId,
    pub channels: Vec<ChannelBytes>,
    pub reference: Option<OutputRef>,
    pub failure: Option<StorageFailure>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelBytes {
    pub channel: Channel,
    pub captured: u64,
    pub saved: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadReceipt {
    pub consumer: String,
    pub task: crate::task::TaskId,
    pub channel: Channel,
    pub start: u64,
    pub end: u64,
    pub total: u64,
    pub terminal: bool,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadProgress {
    pub consumer: String,
    pub task: crate::task::TaskId,
    pub channel: Channel,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputId(uuid::Uuid);

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

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutputOwner {
    pub workspace_id: String,
    pub session_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Stdout,
    Stderr,
    Result,
    Log,
}

impl Channel {
    pub const ALL: [Self; 4] = [Self::Stdout, Self::Stderr, Self::Result, Self::Log];

    pub fn file_name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout.txt",
            Self::Stderr => "stderr.txt",
            Self::Result => "result.json",
            Self::Log => "log.txt",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Result => "result",
            Self::Log => "log",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageFailure {
    ResultLimit,
    SessionQuota,
    ServiceQuota,
    ObjectLimit,
    Io,
    FinalizeTimeout,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputChannelRef {
    pub channel: Channel,
    pub path: PathBuf,
    pub captured_bytes: u64,
    pub saved_bytes: u64,
}

/// Only published after the files and manifest have passed the persistence barrier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRef {
    pub id: OutputId,
    pub channels: Vec<OutputChannelRef>,
    pub complete: bool,
    pub failure: Option<StorageFailure>,
    pub sealed_at: jiff::Timestamp,
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

#[derive(Clone)]
pub struct OutputRuntime {
    pub store: Arc<dyn OutputStore>,
    pub owner: OutputOwner,
    pub ptc: PtcResourceLimits,
}

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
