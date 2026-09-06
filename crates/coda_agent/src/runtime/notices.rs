use super::*;
use crate::agent::Receiver;
use coda_core::task::TaskId;

impl AgentRuntime {
    /// Persist the root opening and its receipt before waking the driver.
    pub(crate) async fn admit_background_notice(
        &self,
        root_name: String,
        task_id: TaskId,
        outcomes: Vec<coda_core::llm::TaskNoticeOutcome>,
        content: String,
    ) -> Result<bool, String> {
        let pending = self
            .executions
            .lock()
            .expect("executions")
            .notice_wakeups
            .get(&task_id)
            .cloned();
        let accepted = self
            .session_storage
            .has_notice_receipt(task_id.clone())
            .await?;
        if accepted && pending.is_none() {
            return Ok(false);
        }
        let message_id = task_id.notice_message_id();
        let turn = TurnId::from(message_id);
        let envelope = if let Some(envelope) = pending {
            envelope
        } else {
            self.turn_gate
                .open(turn)
                .map_err(|_| "a root turn is already active")?;
            let envelope = Envelope::with_id(|id| Envelope {
                id,
                from: Sender::User,
                to: Receiver {
                    name: root_name.clone(),
                    thread_id: ThreadId::from(self.session_id.clone()),
                },
                reply_to: None,
                body: EnvelopeBody::Task {
                    message_id,
                    task: content.clone(),
                    images: vec![],
                    notice: Some(outcomes.clone()),
                },
            });
            if let Err(error) = self.register_execution(&envelope) {
                self.turn_gate.close(turn);
                return Err(error.to_string());
            }
            self.executions
                .lock()
                .expect("executions")
                .notice_wakeups
                .insert(task_id.clone(), envelope.clone());
            envelope
        };
        if !accepted {
            let opening = async {
                let mut checkpoint = self
                    .session_storage
                    .load_checkpoint(&self.session_id)
                    .await?
                    .unwrap_or(StoredCheckpoint {
                        thread_id: self.session_id.clone(),
                        agent_name: root_name,
                        parent_thread_id: None,
                        derivation_key: None,
                        active_execution: None,
                        messages: vec![],
                        resume_point: StoredResumePoint::Generation,
                        suspended_at: jiff::Timestamp::default(),
                    });
                checkpoint.active_execution = self.execution(&envelope.to.thread_id);
                checkpoint.messages.push(crate::HistoryEntry::new(
                    turn,
                    coda_core::llm::Message::TaskNotice(coda_core::llm::TaskNoticeMessage::new(
                        message_id, outcomes, content,
                    )),
                ));
                self.session_storage
                    .admit_task_notice(task_id.clone(), checkpoint)
                    .await
            }
            .await;
            if let Err(error) = opening {
                match self
                    .session_storage
                    .has_notice_receipt(task_id.clone())
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        self.executions
                            .lock()
                            .expect("executions")
                            .notice_wakeups
                            .remove(&task_id);
                        self.turn_gate.close(turn);
                        return Err(error);
                    }
                    // Keep the slot reserved until a retry can establish whether
                    // the transaction committed; no other root turn may overlap it.
                    Err(_) => return Err(error),
                }
            }
        }
        self.deliver(envelope).await.map_err(|e| e.to_string())?;
        self.executions
            .lock()
            .expect("executions")
            .notice_wakeups
            .remove(&task_id);
        Ok(true)
    }
}
