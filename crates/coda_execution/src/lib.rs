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
//! Payload collection and retention belong to `coda_output`. This crate saves
//! task lifecycle metadata and reads bounded pages without consuming them until
//! their delivery receipts have committed.

use coda_output::archive_dir;
mod archived_tasks;
mod inventory;
mod manifest;
pub mod process;
mod registry;
mod task_archive;

pub use archive_dir::{ArchiveDir, ArchiveError, ArchiveFileName, ArchiveRootLock};
pub use archived_tasks::ArchivedTasks;
pub use coda_core::task::{InvalidTaskId, TaskId};
pub use inventory::{ArchiveInventory, scan_inventory};
pub use manifest::{ExpireReason, TaskOutputManifest};
pub use process::{GroupedChild, PIPE_DRAIN_TIMEOUT};
pub use registry::*;
pub use task_archive::{
    TaskArchive, TaskCommitGuard, TaskOutputFiles, TaskPersistentState, TaskRecord,
};

mod output;

mod read_page;
pub use read_page::{TaskPage, TaskResultCursor, TaskResultPage};
