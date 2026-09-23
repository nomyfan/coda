//! Bounded lifecycle recovery inventory; coda_output owns payload accounting.
use crate::archive_dir::EntryKind;
use crate::manifest::{NoticeDelivery, TaskOutputManifest};
use crate::task_archive::load_task_dir;
use crate::{ArchiveDir, ArchiveError, TaskId, TaskStatus, TaskSummary};
#[derive(Default, Debug)]
pub struct ArchiveInventory {
    pub recent_terminal: Vec<TaskSummary>,
    pub subagents: Vec<TaskId>,
    pub recoverable_running: Vec<TaskId>,
    pub spawn_blocked: bool,
}
pub fn scan_inventory(root: &ArchiveDir) -> Result<ArchiveInventory, ArchiveError> {
    let mut inventory = ArchiveInventory::default();
    for entry in root.entries()? {
        let entry = entry?;
        let loaded = (|| {
            if !matches!(entry.kind, EntryKind::Dir | EntryKind::Unknown) {
                return Err(ArchiveError::corrupt("unsafe task archive entry"));
            }
            let id = entry
                .name
                .parse::<TaskId>()
                .map_err(|e| ArchiveError::corrupt(e.to_string()))?;
            let (_, manifest) = load_task_dir(root, &id)?
                .ok_or_else(|| ArchiveError::corrupt("missing task archive"))?;
            Ok((id, manifest))
        })();
        let (id, manifest) = match loaded {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "invalid task archive");
                inventory.spawn_blocked = true;
                continue;
            }
        };
        if manifest.meta.is_subagent()
            && (manifest.status.is_running()
                || manifest.cleanup_pending
                || manifest.notice == Some(NoticeDelivery::Pending))
        {
            if inventory.subagents.len() < 512 {
                inventory.subagents.push(id.clone());
            } else {
                inventory.spawn_blocked = true;
            }
        }
        if manifest.status.is_running() {
            if inventory.recoverable_running.len() < 512 {
                inventory.recoverable_running.push(id);
            } else {
                inventory.spawn_blocked = true;
            }
        } else {
            inventory.recent_terminal.push(summary_of(&id, &manifest));
            compact_recent(&mut inventory.recent_terminal);
        }
    }
    Ok(inventory)
}
pub(crate) fn summary_of(id: &TaskId, manifest: &TaskOutputManifest) -> TaskSummary {
    TaskSummary {
        kind: manifest.meta.kind.clone(),
        parent_task_id: manifest.meta.parent_task_id.clone(),
        subtree_active: manifest.status.is_running(),
        result_available: manifest.meta.is_subagent()
            && manifest.payload.reference.is_some()
            && matches!(manifest.status, TaskStatus::Completed { .. }),
        id: id.as_str().to_owned(),
        command: manifest.meta.command().to_owned(),
        description: manifest.meta.description.clone(),
        agent_name: manifest.meta.agent_name().to_owned(),
        status: manifest.status.clone(),
        started_at: manifest.started_at,
    }
}

pub(crate) fn compact_recent(summaries: &mut Vec<TaskSummary>) {
    summaries.sort_by_key(|s| std::cmp::Reverse(s.status.terminal_at().unwrap_or(s.started_at)));
    summaries.truncate(32);
}
