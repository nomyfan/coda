//! Task lifecycle metadata. Payloads are owned by the shared output store.

use serde::{Deserialize, Serialize};

use super::TaskStatus;
use coda_core::task::TaskId;

/// Current on-disk manifest format. A future breaking change bumps this;
/// an unknown version is treated as corrupt.
pub const MANIFEST_VERSION: u32 = 4;

/// Full manifest persisted as `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutputManifest {
    pub payload: coda_core::output::OutputSnapshot,
    pub notice: Option<NoticeDelivery>,
    pub cleanup_pending: bool,
    pub scope_members: Vec<coda_core::task::ScopeMember>,
    pub manifest_version: u32,
    pub id: TaskId,
    pub meta: super::TaskMeta,
    pub started_at: jiff::Timestamp,
    pub terminal_at: Option<jiff::Timestamp>,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpireReason {
    SessionQuota,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeDelivery {
    Pending,
    Delivered,
}
