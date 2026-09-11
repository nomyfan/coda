//! Read persisted results without task recovery, quota cleanup or notice delivery.

use std::io::Read;
use std::path::Path;

use crate::quota::{compact_recent, summary_of};
use crate::task_archive::load_task_dir;
use crate::{
    ArchiveDir, ArchiveError, ArchiveFileName, DiskTail, SESSION_QUOTA_BYTES, TaskId, TaskResult,
    TaskResultOutput, TaskStatus, TaskSummary,
};

pub struct ArchivedTasks {
    root: ArchiveDir,
}

impl ArchivedTasks {
    /// Missing archives return None; invalid or inaccessible archives return an error.
    pub fn open_existing(path: &Path) -> Result<Option<Self>, ArchiveError> {
        Ok(ArchiveDir::open_existing_root(path)?.map(|root| Self { root }))
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
        let root = self.root.clone();
        let id = id.clone();
        let loaded = tokio::task::spawn_blocking(move || load_task_dir(&root, &id))
            .await
            .map_err(|error| ArchiveError::corrupt(error.to_string()))??;
        let Some((dir, manifest)) = loaded else {
            return Ok(None);
        };
        let status = manifest.status;
        if status.is_running() {
            return Ok(Some(TaskResult::Pending { status }));
        }
        if !manifest.output.rings_present() {
            return Ok(Some(TaskResult::Expired { status }));
        }
        let output = if manifest.meta.is_subagent() {
            let answer = if matches!(status, TaskStatus::Completed { .. }) {
                tokio::task::spawn_blocking(move || {
                    let file = dir.open_file(ArchiveFileName::Result, false)?;
                    if manifest.result_bytes > SESSION_QUOTA_BYTES
                        || file.metadata()?.len() != manifest.result_bytes
                    {
                        return Err(ArchiveError::corrupt(
                            "result length disagrees with manifest",
                        ));
                    }
                    let mut answer = String::new();
                    file.take(manifest.result_bytes + 1)
                        .read_to_string(&mut answer)?;
                    Ok::<_, ArchiveError>(answer)
                })
                .await
                .map_err(|error| ArchiveError::corrupt(error.to_string()))??
            } else {
                status.describe()
            };
            TaskResultOutput::Subagent { answer }
        } else {
            let capacities = (manifest.stdout.capacity, manifest.stderr.capacity);
            let (stdout, stderr) = tokio::task::spawn_blocking(move || {
                let open = |name, stream: &crate::StreamManifest| {
                    DiskTail::reopen(
                        dir.open_file(name, false)?,
                        stream.capacity,
                        stream.start_offset,
                        stream.total_written,
                    )
                    .map_err(ArchiveError::from)
                };
                Ok::<_, ArchiveError>((
                    open(ArchiveFileName::StdoutRing, &manifest.stdout)?,
                    open(ArchiveFileName::StderrRing, &manifest.stderr)?,
                ))
            })
            .await
            .map_err(|error| ArchiveError::corrupt(error.to_string()))??;
            let stdout = stdout.read_from(0, capacities.0 as usize).await?;
            let stderr = stderr.read_from(0, capacities.1 as usize).await?;
            TaskResultOutput::Shell {
                stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
                stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
                stdout_overwritten: stdout.lost,
                stderr_overwritten: stderr.lost,
            }
        };
        Ok(Some(TaskResult::Available { status, output }))
    }
}

#[cfg(test)]
#[path = "archived_tasks_tests.rs"]
mod tests;
