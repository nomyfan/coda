use super::*;
use crate::PendingApproval;
use crate::execution::{CompletionTarget, ExecutionIdentity, ProcessGroupId, StoredExecution};
use coda_core::{
    llm::{MessageOrigin, ToolOutput},
    task::{ScopeMember, TaskId},
};
use coda_execution::{TaskExit, TaskKind, TaskMeta, TaskOrigin};
use std::collections::{HashMap, HashSet};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(super) struct LiveExecution {
    pub stored: StoredExecution,
    pub cancel: CancellationToken,
}

pub(super) struct BackgroundGroup {
    pub members: Vec<ScopeMember>,
    pub completion: Option<oneshot::Sender<ToolOutput>>,
    pub closed: bool,
    pub stopping: bool,
    pub reason: Option<String>,
    pub stopped: tokio::sync::watch::Sender<bool>,
}

#[derive(Default)]
pub(super) struct Executions {
    pub processes: HashMap<String, LiveExecution>,
    pub closing: bool,
    pub notice_wakeups: HashMap<TaskId, Envelope>,
    pub background: HashMap<TaskId, BackgroundGroup>,
    pub quarantined: HashSet<String>,
    pub approvals: HashMap<(String, coda_core::llm::MessageId), PendingApproval>,
}

impl ProcessRuntime {
    pub(crate) fn root_turn_active(&self) -> bool {
        self.turn_gate.active_id().is_some()
            && self
                .executions
                .lock()
                .expect("executions")
                .notice_wakeups
                .is_empty()
    }

    pub fn pending_approvals(&self) -> Vec<PendingApproval> {
        self.executions
            .lock()
            .expect("executions")
            .approvals
            .values()
            .cloned()
            .collect()
    }

    pub fn has_background_work(&self) -> bool {
        let state = self.executions.lock().expect("executions");
        !state.quarantined.is_empty() || !state.background.is_empty()
    }

    pub(crate) async fn stop_background(&self) {
        let ids: Vec<_> = {
            let mut state = self.executions.lock().expect("executions");
            state.closing = true;
            state.background.keys().cloned().collect()
        };
        for id in ids {
            if let Some(background) = &self.background {
                let _ = background.request_kill(&id).await;
                background.wait_terminal(&id).await;
            }
        }
    }

    pub(crate) async fn persist_group_members(&self, id: &TaskId) -> Result<(), String> {
        let members = self
            .executions
            .lock()
            .expect("executions")
            .background
            .get(id)
            .ok_or("scope is closed")?
            .members
            .clone();
        self.background
            .as_ref()
            .ok_or("background registry is unavailable")?
            .record_scope(id, members, false)
            .await
            .map_err(|e| e.to_string())
    }

    async fn retire_background_group(&self, id: &TaskId) -> bool {
        let (members, stopped) = {
            let mut state = self.executions.lock().expect("executions");
            let Some(scope) = state.background.get_mut(id) else {
                return false;
            };
            if scope.stopping {
                return false;
            }
            scope.stopping = true;
            let stopped = scope.stopped.clone();
            let candidates = scope.members.clone();
            let members: Vec<_> = candidates
                .iter()
                .filter(|member| {
                    state
                        .processes
                        .get(&member.pid)
                        .is_some_and(|e| e.stored.invocation_id == member.invocation_id)
                })
                .cloned()
                .collect();
            for member in &members {
                state.quarantined.insert(member.pid.clone());
            }
            (members, stopped)
        };
        for member in &members {
            let handle = self.processes.lock().await.get(&member.pid).cloned();
            if let Some(mut handle) = handle {
                let _ = handle.send_command(ProcessControl::StopGroup).await;
                if !*handle.finished.borrow()
                    && timeout(Duration::from_secs(3), handle.finished.changed())
                        .await
                        .is_err()
                {
                    handle.abort.abort();
                }
                while !*handle.finished.borrow_and_update()
                    && handle.finished.changed().await.is_ok()
                {}
                self.processes.lock().await.remove(&member.pid);
            }
        }
        let mut state = self.executions.lock().expect("executions");
        for member in members {
            state.processes.remove(&member.pid);
            state.quarantined.remove(&member.pid);
        }
        state.background.remove(id);
        stopped.send_replace(true);
        true
    }

    pub(crate) async fn refresh_approval_status(&self, id: &TaskId) {
        let _gate = self.approval_status_gate.lock().await;
        let waiting = self
            .executions
            .lock()
            .expect("executions")
            .approvals
            .values()
            .any(|a| a.task_id.as_ref() == Some(id));
        if let Some(background) = &self.background
            && let Err(error) = background.set_waiting_approval(id, waiting).await
        {
            let runtime = self.clone();
            let id = id.clone();
            tokio::spawn(async move {
                runtime
                    .stop_background_group(&id, Some(error.to_string()))
                    .await;
            });
        }
    }

    pub(crate) fn execution(&self, thread: &ProcessId) -> Option<StoredExecution> {
        self.executions
            .lock()
            .expect("executions")
            .processes
            .get(thread.as_ref())
            .map(|e| e.stored.clone())
    }

    pub(crate) fn execution_cancel(&self, thread: &ProcessId) -> CancellationToken {
        self.executions
            .lock()
            .expect("executions")
            .processes
            .get(thread.as_ref())
            .map(|e| e.cancel.child_token())
            .unwrap_or_default()
    }

    pub(crate) fn execution_stopped(&self, thread: &ProcessId) -> bool {
        self.executions
            .lock()
            .expect("executions")
            .processes
            .get(thread.as_ref())
            .is_some_and(|e| e.cancel.is_cancelled())
    }

    pub(crate) fn restore_execution(&self, thread: &ProcessId, stored: StoredExecution) {
        self.executions
            .lock()
            .expect("executions")
            .processes
            .entry(thread.0.clone())
            .or_insert_with(|| LiveExecution {
                stored,
                cancel: CancellationToken::new(),
            });
    }

    pub(super) fn register_execution(&self, envelope: &Envelope) -> Result<(), SendCommandError> {
        let mut state = self.executions.lock().expect("executions");
        if state.quarantined.contains(envelope.to.pid.as_ref()) {
            return Err(SendCommandError::AwaitingCleanup);
        }
        let stored = match &envelope.body {
            EnvelopeBody::Task { message_id, .. } => {
                if !state.approvals.is_empty() {
                    return Err(SendCommandError::PendingApprovals);
                }
                StoredExecution {
                    invocation_id: envelope.id.clone(),
                    scope: ProcessGroupId::Foreground {
                        turn_id: TurnId::from(*message_id),
                    },
                    completion: CompletionTarget::RootTurn,
                    agent_path: vec![envelope.to.name.clone()],
                }
            }
            EnvelopeBody::ToolCall { turn_id, .. } => {
                if state
                    .processes
                    .get(envelope.to.pid.as_ref())
                    .is_some_and(|e| e.stored.invocation_id == envelope.id)
                {
                    return Ok(());
                }
                let mut path = vec![];
                let scope = match &envelope.from {
                    Sender::Agent { pid, .. } => state.processes.get(pid.as_ref()).map(|e| {
                        path = e.stored.agent_path.clone();
                        e.stored.scope.clone()
                    }),
                    _ => None,
                }
                .unwrap_or(ProcessGroupId::Foreground { turn_id: *turn_id });
                if let ProcessGroupId::Background { task_id } = &scope {
                    let scope = state
                        .background
                        .get_mut(task_id)
                        .ok_or(SendCommandError::ScopeClosed)?;
                    if scope.closed {
                        return Err(SendCommandError::ScopeClosed);
                    }
                    scope.members.push(ScopeMember {
                        pid: envelope.to.pid.0.clone(),
                        invocation_id: envelope.id.clone(),
                    });
                }
                path.push(envelope.to.name.clone());
                StoredExecution {
                    invocation_id: envelope.id.clone(),
                    scope,
                    completion: CompletionTarget::Caller(
                        driver::reply_target_from_envelope(envelope)
                            .expect("tool call has a caller"),
                    ),
                    agent_path: path,
                }
            }
            EnvelopeBody::Resume(_) | EnvelopeBody::Reply { .. } => {
                if state
                    .processes
                    .get(envelope.to.pid.as_ref())
                    .is_some_and(|e| {
                        e.stored.background_task().is_some() && e.cancel.is_cancelled()
                    })
                {
                    return Err(SendCommandError::ScopeClosed);
                }
                return Ok(());
            }
        };
        state.processes.insert(
            envelope.to.pid.0.clone(),
            LiveExecution {
                stored,
                cancel: CancellationToken::new(),
            },
        );
        Ok(())
    }

    pub(crate) async fn dispatch_background(
        &self,
        envelope: Envelope,
        origin: MessageOrigin,
        parent: ProcessId,
    ) -> Result<TaskId, String> {
        if self.exit_barrier.is_exiting() {
            return Err("runtime is shutting down".into());
        }
        if !self.is_root_process(&parent) {
            return Err("only the root thread can start a background subagent".into());
        }
        let background = self
            .background
            .clone()
            .ok_or("background registry is unavailable")?;
        let id = TaskId::for_call(&self.session_id, parent.as_ref(), &origin);
        if background.contains(&id).await.map_err(|e| e.to_string())? {
            return Ok(id);
        }
        let mut path = self
            .execution(&parent)
            .map(|e| e.agent_path)
            .unwrap_or_default();
        path.push(envelope.to.name.clone());
        let (sender, receiver) = oneshot::channel();
        let thread = envelope.to.pid.clone();
        {
            let mut executions = self.executions.lock().expect("executions");
            if executions.closing {
                return Err("runtime is shutting down".into());
            }
            if executions.quarantined.contains(thread.as_ref()) {
                return Err("thread is waiting for abort cleanup".into());
            }
            if !self.calls.try_begin(&thread) {
                return Err("subagent thread is busy".into());
            }
            let member = ScopeMember {
                pid: thread.0.clone(),
                invocation_id: envelope.id.clone(),
            };
            executions.background.insert(
                id.clone(),
                BackgroundGroup {
                    members: vec![member],
                    completion: Some(sender),
                    closed: false,
                    stopping: false,
                    reason: None,
                    stopped: tokio::sync::watch::channel(false).0,
                },
            );
            executions.processes.insert(
                thread.0.clone(),
                LiveExecution {
                    stored: StoredExecution {
                        invocation_id: envelope.id.clone(),
                        scope: ProcessGroupId::Background {
                            task_id: id.clone(),
                        },
                        completion: CompletionTarget::BackgroundTask(id.clone()),
                        agent_path: path.clone(),
                    },
                    cancel: CancellationToken::new(),
                },
            );
        }
        let runtime = self.clone();
        let task_id = id.clone();
        let meta = TaskMeta {
            kind: TaskKind::Subagent {
                agent_name: envelope.to.name.clone(),
            },
            description: match &envelope.body {
                EnvelopeBody::ToolCall { task, .. } => task.clone(),
                _ => unreachable!(),
            },
            parent_task_id: None,
            origin: TaskOrigin {
                pid: parent.0,
                message_origin: Some(origin),
                agent_path: path.clone(),
            },
        };
        let spawned = background.spawn_identified(id.clone(), meta, move |context| async move {
            if let Err(error) = runtime.persist_group_members(&task_id).await {
                runtime.stop_background_group(&task_id, Some(error)).await;
            } else if let Err(error) = runtime.deliver(envelope).await { runtime.stop_background_group(&task_id, Some(error.to_string())).await; }
            let cancelled = context.cancelled();
            let output = tokio::select! {
                biased;
                result = receiver => result.unwrap_or_else(|_| ToolOutput::Err("background execution lost its completion".into())),
                _ = cancelled.cancelled() => {
                    runtime.stop_background_group(&task_id, None).await;
                    ToolOutput::Err(runtime.executions.lock().expect("executions").background.get(&task_id).and_then(|s| s.reason.clone()).unwrap_or_else(|| "Killed by user".into()))
                }
            };
            match output {
                ToolOutput::Ok(answer) => TaskExit::Completed { answer },
                ToolOutput::Err(message) if message == "Killed by user" => TaskExit::Killed,
                ToolOutput::Err(message) => TaskExit::Failed { message },
            }
        }).await;
        if let Err(error) = spawned {
            let mut state = self.executions.lock().expect("executions");
            state.background.remove(&id);
            state.processes.remove(thread.as_ref());
            self.calls.end(&thread);
            return Err(error.to_string());
        }
        let runtime = self.clone();
        let task = id.clone();
        self.process_tasks
            .lock()
            .expect("runtime tasks")
            .spawn(async move {
                background.wait_terminal(&task).await;
                if runtime.retire_background_group(&task).await {
                    runtime.calls.end(&thread);
                }
                format!("background scope {task}")
            });
        Ok(id)
    }

    pub(crate) fn checkpoint_failed(&self, identity: ExecutionIdentity, error: String) {
        let task = self
            .executions
            .lock()
            .expect("executions")
            .processes
            .get(&identity.pid)
            .filter(|e| e.stored.invocation_id == identity.invocation_id)
            .and_then(|e| e.stored.background_task().cloned());
        if let Some(task) = task {
            {
                let mut state = self.executions.lock().expect("executions");
                if let Some(scope) = state.background.get_mut(&task) {
                    scope.closed = true;
                    scope.reason = Some(error.clone());
                }
                for execution in state
                    .processes
                    .values()
                    .filter(|e| e.stored.background_task() == Some(&task))
                {
                    execution.cancel.cancel();
                }
            }
            let runtime = self.clone();
            tokio::spawn(async move {
                runtime.stop_background_group(&task, Some(error)).await;
            });
        }
    }

    pub(crate) fn complete_background(&self, execution: &StoredExecution, output: ToolOutput) {
        if let CompletionTarget::BackgroundTask(id) = &execution.completion {
            let mut state = self.executions.lock().expect("executions");
            if let Some(scope) = state.background.get_mut(id)
                && !scope.closed
            {
                scope.closed = true;
                if let Some(sender) = scope.completion.take() {
                    let _ = sender.send(output);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "groups_tests.rs"]
mod tests;
