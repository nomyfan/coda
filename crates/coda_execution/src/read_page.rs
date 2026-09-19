use crate::{BackgroundTasks, TaskAccessError, TaskId};
use coda_core::output::{Channel, OutputError, ReadProgress, ReadReceipt};

pub struct TaskPage {
    pub body: String,
    pub references: Vec<coda_core::output::OutputRef>,
    pub receipts: Vec<ReadReceipt>,
    pub complete: bool,
}

impl BackgroundTasks {
    pub async fn output_progress(&self, consumer: &str, task: &TaskId, channel: Channel) -> u64 {
        self.progress
            .lock()
            .await
            .iter()
            .find(|p| p.consumer == consumer && &p.task == task && p.channel == channel)
            .map_or(0, |p| p.offset)
    }

    pub async fn restore_output_progress(&self, entries: Vec<ReadProgress>) {
        let mut progress = self.progress.lock().await;
        for entry in entries {
            if let Some(current) = progress.iter_mut().find(|p| {
                p.consumer == entry.consumer && p.task == entry.task && p.channel == entry.channel
            }) {
                current.offset = current.offset.max(entry.offset);
            } else {
                progress.push(entry);
            }
        }
    }

    /// Called only after a successful checkpoint transaction.
    pub async fn commit_reads(&self, receipts: &[ReadReceipt]) {
        let mut progress = self.progress.lock().await;
        for receipt in receipts {
            if let Some(current) = progress.iter_mut().find(|p| {
                p.consumer == receipt.consumer
                    && p.task == receipt.task
                    && p.channel == receipt.channel
            }) {
                if receipt.start <= current.offset {
                    current.offset = current.offset.max(receipt.end);
                }
            } else if receipt.start == 0 {
                progress.push(ReadProgress {
                    consumer: receipt.consumer.clone(),
                    task: receipt.task.clone(),
                    channel: receipt.channel,
                    offset: receipt.end,
                });
            }
        }
    }

    pub async fn read_page(
        &self,
        id: &TaskId,
        consumer: &str,
        positions: [u64; 3],
        byte_offset: Option<u64>,
        budget: usize,
    ) -> Result<Option<TaskPage>, TaskAccessError> {
        let Some(record) = self.backend.archive.open(id).await? else {
            return Ok(None);
        };
        let guard = record.lock_commit().await;
        let status = guard.current().status.clone();
        let snapshot = record.files().snapshot();
        let reference: Vec<_> = snapshot.reference.clone().into_iter().collect();
        let mut status_text = status.describe();
        if status_text.len() > 256 {
            let mut end = 256;
            while !status_text.is_char_boundary(end) {
                end -= 1;
            }
            status_text.truncate(end);
        }
        let mut body = format!("status: {status_text}");
        let saved = coda_core::output::describe_saved(&reference, snapshot.failure.as_ref());
        // Headings, one truncation line per channel and the saved-file lines.
        let metadata = body.len() + saved.len() + 512;
        if metadata + 24 > budget {
            return Err(std::io::Error::other(OutputError::PageLimit(
                "budget cannot hold task metadata".into(),
            ))
            .into());
        }
        if snapshot.sealed && snapshot.reference.is_none() {
            let failure = snapshot
                .failure
                .unwrap_or(coda_core::output::StorageFailure::Incomplete);
            let mut preview = coda_output::preview::Preview::new(budget - metadata);
            preview.append(snapshot.preview.as_bytes());
            body.push_str(&format!(
                "\noutput preview:\n{}\n{}",
                preview.text(),
                coda_core::output::describe_saved(&[], Some(&failure))
            ));
            return Ok(Some(TaskPage {
                body,
                references: reference,
                receipts: vec![],
                complete: false,
            }));
        }
        let subagent = record.meta().is_subagent();
        if !subagent && byte_offset.is_some() {
            return Err(std::io::Error::other("byte_offset is only supported for subagent results; shell reads are incremental per consumer").into());
        }
        let channels: &[Channel] = if subagent {
            &[Channel::Result]
        } else {
            &[Channel::Stdout, Channel::Stderr]
        };
        let share = (budget - metadata) / channels.len();
        let mut receipts = Vec::new();
        let mut notes = Vec::new();
        let mut shown_any = false;
        let mut complete = !status.is_running() && snapshot.failure.is_none();
        for (index, channel) in channels.iter().enumerate() {
            let position_index = if subagent { 2 } else { index };
            let start = if subagent {
                byte_offset.unwrap_or(0)
            } else {
                positions[position_index]
            };
            let stream = match channel {
                Channel::Stdout => &record.files().stdout,
                Channel::Stderr => &record.files().stderr,
                _ => &record.files().result,
            };
            let page = stream.read_from(start, share).await?;
            let (text, used) = coda_output::preview::decode_within(&page.bytes, share);
            let end = start + used as u64;
            let total = snapshot
                .channels
                .iter()
                .find(|c| c.channel == *channel)
                .map_or(0, |c| c.captured);
            complete &= end >= total && start <= positions[position_index];
            if !text.is_empty() {
                shown_any = true;
                let heading = match (channel, start) {
                    (Channel::Result, 0) => "result:".to_owned(),
                    (Channel::Result, _) => format!("result (from byte {start}):"),
                    _ => format!("{} (new):", channel.name()),
                };
                body.push_str(&format!("\n{heading}\n{text}"));
            }
            if end < total {
                let next = if subagent {
                    format!("continue with byte_offset={end}")
                } else {
                    "call task_output again for the rest".to_owned()
                };
                notes.push(format!(
                    "[{} truncated: longer than the {budget}-byte output limit; {next}]",
                    channel.name()
                ));
            }
            receipts.push(ReadReceipt {
                consumer: consumer.into(),
                task: id.clone(),
                channel: *channel,
                start,
                end,
                total,
                terminal: !status.is_running(),
                complete: false,
            });
        }
        for receipt in &mut receipts {
            receipt.complete = complete;
        }
        if !shown_any {
            body.push_str("\n(no new output)");
        }
        for note in notes {
            body.push('\n');
            body.push_str(&note);
        }
        if !saved.is_empty() {
            body.push('\n');
            body.push_str(&saved);
        }
        if body.len() > budget {
            return Err(std::io::Error::other(OutputError::PageLimit(
                "task page exceeded the assigned budget".into(),
            ))
            .into());
        }
        Ok(Some(TaskPage {
            body,
            references: reference,
            receipts,
            complete,
        }))
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TaskResultCursor {
    #[serde(default)]
    pub stdout: u64,
    #[serde(default)]
    pub stderr: u64,
    #[serde(default)]
    pub result: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TaskResultPage {
    pub next: Option<TaskResultCursor>,
    pub output_refs: Vec<coda_core::output::OutputRef>,
    pub complete: bool,
    pub storage_failure: Option<coda_core::output::StorageFailure>,
}

pub(crate) async fn read_result(
    record: &std::sync::Arc<crate::task_archive::TaskRecord>,
    cursor: TaskResultCursor,
) -> Result<crate::TaskResult, crate::ArchiveError> {
    let guard = record.lock_commit().await;
    let status = guard.current().status.clone();
    if status.is_running() {
        return Ok(crate::TaskResult::Pending { status });
    }
    let snapshot = record.files().snapshot();
    if snapshot.reference.is_none() {
        return Ok(crate::TaskResult::Available {
            status,
            output: crate::TaskResultOutput::Subagent {
                answer: snapshot.preview,
            },
            page: TaskResultPage {
                next: None,
                output_refs: vec![],
                complete: false,
                storage_failure: snapshot
                    .failure
                    .or(Some(coda_core::output::StorageFailure::Incomplete)),
            },
        });
    }
    let limit = 4 * 1024;
    let mut next = cursor;
    let mut more = false;
    let output = if record.meta().is_subagent() {
        let page = record
            .files()
            .result
            .read_from(cursor.result, limit)
            .await?;
        next.result = page.next_cursor;
        more |= page.has_more;
        crate::TaskResultOutput::Subagent {
            answer: String::from_utf8_lossy(&page.bytes).into_owned(),
        }
    } else {
        let stdout = record
            .files()
            .stdout
            .read_from(cursor.stdout, limit)
            .await?;
        let stderr = record
            .files()
            .stderr
            .read_from(cursor.stderr, limit)
            .await?;
        next.stdout = stdout.next_cursor;
        next.stderr = stderr.next_cursor;
        more |= stdout.has_more || stderr.has_more;
        crate::TaskResultOutput::Shell {
            stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
            stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
            stdout_overwritten: stdout.lost,
            stderr_overwritten: stderr.lost,
        }
    };
    Ok(crate::TaskResult::Available {
        status,
        output,
        page: TaskResultPage {
            next: more.then_some(next),
            output_refs: snapshot.reference.into_iter().collect(),
            complete: !more && snapshot.failure.is_none(),
            storage_failure: snapshot.failure,
        },
    })
}
