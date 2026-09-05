use super::*;
use crate::execution::ScopeAbort;
use coda_core::{llm::ToolOutput, task::TaskId};

impl AgentRuntime {
    pub(crate) async fn stop_scope(&self, id: &TaskId, error: Option<String>) {
        let (owns_stop, mut stopped) = {
            let mut state = self.executions.lock().expect("executions");
            let Some(scope) = state.background.get_mut(id) else {
                return;
            };
            let owns_stop = !scope.stopping;
            scope.stopping = true;
            scope.closed = true;
            if error.is_some() {
                scope.reason = error.clone();
            }
            (owns_stop, scope.stopped.subscribe())
        };
        if !owns_stop {
            while !*stopped.borrow_and_update() && stopped.changed().await.is_ok() {}
            return;
        }
        let (members, removed) = {
            let mut state = self.executions.lock().expect("executions");
            let scope = state.background.get(id).expect("scope owns stop");
            let members = scope.members.clone();
            for member in &members {
                state.quarantined.insert(member.thread_id.clone());
                if let Some(execution) = state.threads.get(&member.thread_id) {
                    execution.cancel.cancel();
                }
            }
            let keys: Vec<_> = state
                .approvals
                .keys()
                .filter(|(thread, _)| members.iter().any(|m| &m.thread_id == thread))
                .cloned()
                .collect();
            let removed: Vec<_> = keys
                .into_iter()
                .filter_map(|key| state.approvals.remove(&key))
                .collect();
            (members, removed)
        };
        for approval in removed {
            let thread = ThreadId::from(approval.thread_id.clone());
            let turn = TurnId::from(approval.parent_message_id);
            let _ = self.global_event_tx.send((
                approval.agent_name,
                thread,
                turn,
                AgentEvent::ApprovalRemoved {
                    thread_id: approval.thread_id,
                    parent_message_id: approval.parent_message_id,
                    task_id: approval.task_id,
                },
            ));
        }
        let Some(background) = &self.background else {
            return;
        };
        let _ = background.request_kill(id).await;
        let _ = background.record_scope(id, members.clone(), true).await;
        let handles: Vec<_> = {
            let drivers = self.agents.lock().await;
            members
                .iter()
                .filter_map(|member| drivers.get(&member.thread_id).cloned())
                .collect()
        };
        for handle in &handles {
            let _ = handle.send_command(AgentControl::StopScope).await;
        }
        for mut handle in handles {
            if !*handle.finished.borrow()
                && timeout(Duration::from_secs(3), handle.finished.changed())
                    .await
                    .is_err()
            {
                handle.abort.abort();
                while !*handle.finished.borrow_and_update()
                    && handle.finished.changed().await.is_ok()
                {}
            }
        }
        background.kill_children(id).await;
        {
            let mut drivers = self.agents.lock().await;
            for member in &members {
                drivers.remove(&member.thread_id);
                self.calls.clear(&ThreadId::from(member.thread_id.clone()));
            }
        }
        {
            let mut snapshot = self.snapshot.lock().await;
            let mut stored = snapshot.clone().into();
            crate::execution::remove_scope_messages(&mut stored, &members);
            *snapshot = stored.into();
        }
        let reason = self
            .executions
            .lock()
            .expect("executions")
            .background
            .get(id)
            .and_then(|s| s.reason.clone())
            .unwrap_or_else(|| "Killed by user".into());
        let abort = ScopeAbort {
            task_id: id.clone(),
            members: members.clone(),
            reason: reason.clone(),
        };
        let runtime = self.clone();
        self.agent_tasks
            .lock()
            .expect("runtime tasks")
            .spawn(async move {
                let mut delay = Duration::from_millis(100);
                loop {
                    if runtime
                        .session_storage
                        .abort_scope(abort.clone())
                        .await
                        .is_ok()
                        && background_cleanup(&runtime, &abort).await.is_ok()
                    {
                        // The monitor must consume the failure reason before this scope
                        // is forgotten or its stateful threads become reusable.
                        runtime
                            .background
                            .as_ref()
                            .expect("background scope")
                            .wait_terminal(&abort.task_id)
                            .await;
                        let mut state = runtime.executions.lock().expect("executions");
                        state.background.remove(&abort.task_id);
                        for member in &abort.members {
                            state.quarantined.remove(&member.thread_id);
                            state.threads.remove(&member.thread_id);
                        }
                        break;
                    }
                    if runtime.exit_barrier.is_exiting() {
                        break;
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
                format!("scope cleanup {}", abort.task_id)
            });
        if let Some(scope) = self
            .executions
            .lock()
            .expect("executions")
            .background
            .get_mut(id)
            && let Some(sender) = scope.completion.take()
        {
            let _ = sender.send(ToolOutput::Err(reason.clone()));
        }
        if let Some(scope) = self
            .executions
            .lock()
            .expect("executions")
            .background
            .get(id)
        {
            scope.stopped.send_replace(true);
        }
    }
}

async fn background_cleanup(runtime: &AgentRuntime, abort: &ScopeAbort) -> Result<(), String> {
    for member in &abort.members {
        runtime
            .session_storage
            .load_checkpoint(&member.thread_id)
            .await?;
    }
    runtime
        .background
        .as_ref()
        .expect("background scope has a registry")
        .record_scope(&abort.task_id, abort.members.clone(), false)
        .await
        .map_err(|e| e.to_string())
}
