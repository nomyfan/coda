//! Running child processes on behalf of an agent, in both tenses.
//!
//! [`process`] holds the primitive every caller shares — [`GroupedChild`], a
//! command pinned inside a killable process group — whether the command is
//! awaited within a turn (`coda_tools`' foreground path: `shell`, `grep`,
//! `glob`, `ls`) or outlives it. It depends on nothing else here.
//!
//! The rest is the background half (design: `docs/design/background-tasks.md`,
//! `docs/design/background-task-output-spool.md`).
//!
//! [`BackgroundTasks`] owns the lifecycle of background work independently
//! of any turn: tasks are started via [`BackgroundTasks::spawn_with`],
//! observed via incremental reads and a summaries watch, torn down via
//! [`kill`](BackgroundTasks::kill) / [`shutdown`](BackgroundTasks::shutdown),
//! and their completions accumulate as [`TaskNotice`]s until a caller drains
//! them for delivery.
//!
//! The registry is generic over what a task *runs* (a boxed future given a
//! [`TaskCtx`]); the process-backed [`BackgroundTasks::spawn`] builds on
//! this same engine. The future seam stays public: cross-crate lifecycle tests
//! drive fake tasks through it, and it is the seam a non-process backend
//! plugs into.
//!
//! Output lives on disk rather than in memory: each task owns a pair of bounded
//! ring files under a session archive, so a long-running task does not hold its
//! output cap in RAM and unread output survives a hub entry release.

mod archive_dir;
mod disk_tail;
mod manifest;
pub mod process;
mod quota;
mod registry;
mod task_archive;

pub use archive_dir::{ArchiveDir, ArchiveError, ArchiveFileName, BackgroundRootLock};
pub use coda_core::task::{InvalidTaskId, TaskId};
pub use disk_tail::{DiskTail, OutputChunk};
pub use manifest::{ExpireReason, OutputDisposition, StreamManifest, TaskOutputManifest};
pub use process::{GroupedChild, PIPE_DRAIN_TIMEOUT};
pub use quota::{
    ArchiveInventory, ExpirationFact, InventoryIssue, QuotaError, QuotaReservation, ReserveOutcome,
    RetainedIndexEntry, SESSION_QUOTA_BYTES, SessionQuota, scan_inventory,
};
pub use registry::*;
pub use task_archive::{
    DEFAULT_STREAM_CAPACITY, TaskArchive, TaskCommitGuard, TaskOutputFiles, TaskPersistentState,
    TaskRecord,
};
