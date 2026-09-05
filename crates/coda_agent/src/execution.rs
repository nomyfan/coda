use crate::agent::ReplyTarget;
use coda_core::{
    llm::TurnId,
    task::{ScopeMember, TaskId},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionScope {
    Foreground { turn_id: TurnId },
    Background { task_id: TaskId },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CompletionTarget {
    RootTurn,
    Caller(ReplyTarget),
    BackgroundTask(TaskId),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredExecution {
    pub invocation_id: String,
    pub scope: ExecutionScope,
    pub completion: CompletionTarget,
    pub agent_path: Vec<String>,
}

impl StoredExecution {
    pub fn reply_target(&self) -> Option<&ReplyTarget> {
        match &self.completion {
            CompletionTarget::Caller(target) => Some(target),
            _ => None,
        }
    }

    pub fn background_task(&self) -> Option<&TaskId> {
        match &self.scope {
            ExecutionScope::Background { task_id } => Some(task_id),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionIdentity {
    pub thread_id: String,
    pub invocation_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScopeAbort {
    pub task_id: TaskId,
    pub members: Vec<ScopeMember>,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct CleanupReceipt {
    pub task_id: TaskId,
}

/// Close only persisted calls without results; successful results and recorded state survive.
pub fn abort_checkpoint(checkpoint: &mut crate::StoredCheckpoint, reason: &str) {
    use coda_core::llm::{Message, ToolCallOutcome, ToolMessage, ToolOutput};
    let mut pending = Vec::new();
    for entry in &checkpoint.messages {
        match &entry.message {
            Message::Assistant(assistant) => pending.extend(
                assistant
                    .tool_calls
                    .iter()
                    .cloned()
                    .map(|call| (entry.turn_id, call)),
            ),
            Message::Tool(tool) => {
                if let Some(index) = pending.iter().position(|(_, call)| call.id == tool.id) {
                    pending.remove(index);
                }
            }
            _ => {}
        }
    }
    checkpoint
        .messages
        .extend(pending.into_iter().map(|(turn, call)| {
            crate::HistoryEntry::new(
                turn,
                Message::Tool(ToolMessage::new(
                    call.id,
                    call.name,
                    ToolOutput::Err(reason.into()),
                    ToolCallOutcome::Aborted,
                    None,
                )),
            )
        }));
    checkpoint.resume_point = crate::persist::StoredResumePoint::Generation;
    checkpoint.active_execution = None;
}

pub fn remove_scope_messages(snapshot: &mut crate::StoredRuntimeSnapshot, members: &[ScopeMember]) {
    let belongs = |thread: &str| members.iter().any(|member| member.thread_id == thread);
    snapshot.active_threads.retain(|thread, _| !belongs(thread));
    for (thread, envelopes) in snapshot
        .drained_envelopes
        .iter_mut()
        .chain(snapshot.agent_drained_envelopes.iter_mut())
    {
        if belongs(thread) {
            envelopes.clear();
        } else {
            envelopes.retain(|envelope| !belongs(envelope.to.thread_id.as_ref()) && !matches!(&envelope.from, crate::Sender::Agent { thread_id, .. } if belongs(thread_id.as_ref())));
        }
    }
}

/// Filter late snapshots without deleting a later invocation on the same stateful thread.
pub fn fence_snapshot(
    snapshot: &mut crate::StoredRuntimeSnapshot,
    aborted: &[ScopeMember],
    active: &std::collections::HashMap<String, String>,
) {
    snapshot.active_threads.retain(|thread, _| {
        !aborted.iter().any(|member| {
            &member.thread_id == thread
                && active
                    .get(thread)
                    .is_none_or(|invocation| invocation == &member.invocation_id)
        })
    });
    for envelopes in snapshot
        .drained_envelopes
        .values_mut()
        .chain(snapshot.agent_drained_envelopes.values_mut())
    {
        envelopes.retain(|envelope| {
            !aborted.iter().any(|member| {
                envelope.id == member.invocation_id
                    || envelope.reply_to.as_ref() == Some(&member.invocation_id)
            })
        });
    }
}
