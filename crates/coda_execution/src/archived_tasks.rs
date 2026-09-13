//! Read persisted results without task recovery, quota cleanup or notice delivery.

use std::path::Path;

use crate::inventory::{compact_recent, summary_of};
use crate::task_archive::load_task_dir;
use crate::{ArchiveDir, ArchiveError, TaskId, TaskResult, TaskSummary};
use std::sync::Arc;

pub struct ArchivedTasks {
    root: ArchiveDir,
    store: Arc<coda_output::Store>,
}

impl ArchivedTasks {
    /// Missing archives return None; invalid or inaccessible archives return an error.
    pub fn open_existing(path: &Path) -> Result<Option<Self>, ArchiveError> {
        Self::open_with_store(path, coda_output::Store::standalone())
    }

    pub fn open_with_store(
        path: &Path,
        store: Arc<coda_output::Store>,
    ) -> Result<Option<Self>, ArchiveError> {
        Ok(ArchiveDir::open_existing_root(path)?.map(|root| Self { root, store }))
    }

    /// A bounded overview of saved statuses. No archived execution is resumed.
    pub async fn overview(&self) -> Result<Vec<TaskSummary>, ArchiveError> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            let mut summaries = Vec::new();
            for entry in root.entries()? {
                let entry = entry?;
                let id: TaskId = entry.name.parse().map_err(|error| {
                    ArchiveError::corrupt(format!("invalid archive entry: {error}"))
                })?;
                let Some((_, manifest)) = load_task_dir(&root, &id)? else {
                    continue;
                };
                let mut summary = summary_of(&id, &manifest);
                summary.subtree_active = false;
                summaries.push(summary);
                compact_recent(&mut summaries);
            }
            Ok(summaries)
        })
        .await
        .map_err(|error| ArchiveError::corrupt(error.to_string()))?
    }

    /// Read a saved result without advancing cursors, acknowledging or repairing it.
    pub async fn read_result(&self, id: &TaskId) -> Result<Option<TaskResult>, ArchiveError> {
        self.read_result_page(id, crate::TaskResultCursor::default())
            .await
    }

    pub async fn read_result_page(
        &self,
        id: &TaskId,
        cursor: crate::TaskResultCursor,
    ) -> Result<Option<TaskResult>, ArchiveError> {
        let archive = crate::task_archive::TaskArchive::with_output(
            self.root.clone(),
            self.store.clone(),
            coda_core::output::OutputOwner {
                workspace_id: String::new(),
                session_id: String::new(),
            },
        );
        let Some(record) = archive.open(id).await? else {
            return Ok(None);
        };
        Ok(Some(crate::read_page::read_result(&record, cursor).await?))
    }
}

#[cfg(test)]
#[path = "archived_tasks_tests.rs"]
mod tests;
