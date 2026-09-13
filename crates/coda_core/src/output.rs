//! Resource limits and metadata shared by output producers and consumers.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod budget;
pub mod config;

pub use budget::{BufferBudget, BufferLease, BufferLimitError};
pub use config::{ModelOutputLimits, OutputLimits, PtcResourceLimits, ResourceLimits};

pub const IO_BLOCK_BYTES: usize = 16 * 1024;
pub const FINALIZE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputId(uuid::Uuid);

impl OutputId {
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
