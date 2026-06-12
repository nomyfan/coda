use std::{
    collections::{HashMap, HashSet, VecDeque},
    vec,
};

use coda_core::llm::{
    ChatCompletionRequest, LLMProvider, LLMStreamEvent, Message, StreamError, ToolCallOutcome,
    ToolMessage, ToolOutput, UserMessage,
};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, warn};

use super::AgentControl;
use crate::{
    AbortedTarget, Agent, AgentEvent, Envelope, PendingApproval, ResumeDecision, RunConfig, Sender,
    SubAgentMode, ThreadId, ToolApprovalMode, ToolCallResolution,
    agent::{
        EnvelopeBody, PendingReply, PendingToolCall, Receiver, ReplyTarget, ResumePoint,
        ToolExecutionState,
    },
    persist::StoredCheckpoint,
    runtime::AgentRuntime,
};

#[instrument(skip_all, fields(agent = %agent.name))]
pub(crate) async fn run_agent(
    runtime: AgentRuntime,
    active: (Option<ThreadId>, Option<ResumeDecision>),
    mut agent: Agent,
    mut control_rx: mpsc::Receiver<AgentControl>,
    mut envelope_rx: mpsc::Receiver<Envelope>,
    config: RunConfig<impl LLMProvider + Clone>,
) {
    info!("Agent {} is running", agent.name);
    let (mut active_thread, resume_decision) = active;
    // When a resume decision is provided alongside the active thread, turn it into
    // a Resume envelope for the first iteration so the agent drops straight from
    // PendingApproval into ToolExecution without re-emitting `Suspended`.
    let mut pending_resume_envelope: Option<Envelope> = match (&active_thread, resume_decision) {
        (Some(tid), Some(decision)) => Some(Envelope::with_id(|id| Envelope {
            id,
            from: Sender::User,
            to: Receiver {
                name: agent.name.clone(),
                thread_id: tid.clone(),
            },
            reply_to: None,
            body: EnvelopeBody::Resume(decision),
        })),
        (None, Some(_)) => {
            warn!(
                "run_agent for {} got a resume decision without an active thread; discarding",
                agent.name
            );
            None
        }
        _ => None,
    };
    if pending_resume_envelope.is_some() {
        // The resume envelope carries the thread_id into the first run; clear the
        // raw active_thread so we don't also trigger a bare `run(None)` that would
        // emit Suspended.
        active_thread = None;
    }
    // When the agent suspends for approval, we clear `active_thread` so the
    // outer loop waits for the next envelope (a Resume or a new Task). But we
    // still need the thread_id available if Exit fires during that wait so the
    // snapshot can record the pending thread for restart-based resume.
    let mut suspended_thread: Option<ThreadId> = None;
    loop {
        // First: if we have a queued resume envelope, run with it.
        // Otherwise: if there's an active thread to continue, just run it without waiting for a new envelope.
        let (thread_id, envelope) = if let Some(envelope) = pending_resume_envelope.take() {
            (envelope.to.thread_id.clone(), Some(envelope))
        } else if let Some(active_thread) = active_thread.take() {
            (active_thread, None)
        } else {
            // Wait for the next envelope, but allow Exit to break the loop.
            let next_envelope = tokio::select! {
                biased;
                cmd = control_rx.recv() => {
                    match cmd {
                        Some(AgentControl::Exit) | None => {
                            // Restore thread_id into active_thread so the
                            // snapshot preserves it for restart-based resume.
                            // None means all senders were dropped; treat it as
                            // an exit signal to avoid a tight spin loop.
                            active_thread = suspended_thread.take();
                            break;
                        }
                        _ => continue,
                    }
                }
                envelope = envelope_rx.recv() => match envelope {
                    Some(e) => {
                        suspended_thread = None;
                        e
                    }
                    None => break,
                },
            };

            (next_envelope.to.thread_id.clone(), Some(next_envelope))
        };

        let cancel = CancellationToken::new();
        active_thread = Some(thread_id.clone());
        let mut agent_loop = AgentLoop {
            runtime: runtime.clone(),
            agent: &mut agent,
            cancel: cancel.clone(),
            config: config.clone(),
            thread_id: thread_id.clone(),
            reply_target: None,
        };
        let mut run_fut = std::pin::pin!(agent_loop.run(envelope));

        // Race the agent loop against incoming control signals.
        let should_exit = tokio::select! {
            biased;
            cmd = control_rx.recv() => {
                let mut should_exit = false;
                match cmd {
                    Some(AgentControl::Abort) | None => {
                        cancel.cancel();
                        active_thread = None;
                    }
                    Some(AgentControl::Exit) => {
                        // Wait the agent loop to exit gracefully.
                        should_exit = true;
                    }
                }
                match (&mut run_fut).await {
                    Ok(TurnOutcome::ExitAcquired | TurnOutcome::Completed) => {
                        active_thread = None;
                    }
                    Ok(TurnOutcome::Suspended) => {
                        // The run ended in suspension; Exit was also requested.
                        // Preserve thread_id in active_thread so the snapshot
                        // records the pending thread for restart-based resume.
                    }
                    Err(err) => {
                        error!("Error in agent loop: {}", err);
                        active_thread = None;
                    }
                }
                should_exit
            }
            ret = &mut run_fut => {
                let mut should_exit = false;
                match ret {
                    Ok(TurnOutcome::ExitAcquired) => {
                        should_exit = true;
                        active_thread = None;
                    }
                    Ok(TurnOutcome::Completed) => {
                        active_thread = None;
                    }
                    Ok(TurnOutcome::Suspended) => {
                        // Agent is now waiting for a Resume envelope. Move the
                        // thread_id to suspended_thread and clear active_thread
                        // so the outer loop falls into the envelope-wait branch.
                        // If Exit arrives during that wait, suspended_thread is
                        // restored into active_thread for the snapshot.
                        suspended_thread = active_thread.take();
                    }
                    Err(err) => {
                        error!("Error in agent loop: {}", err);
                        active_thread = None;
                    }
                }
                should_exit
            }
        };

        if should_exit {
            break;
        }
    }

    info!("Agent {} exiting", agent.name);
    // Drain all remaining envelopes and send them to runtime.
    let mut envelopes = Vec::new();
    while let Ok(envelope) = envelope_rx.try_recv() {
        envelopes.push(envelope);
    }
    runtime
        .save_agent_snapshot(agent.name.clone(), envelopes, active_thread)
        .await;
    info!("Agent {} has exited", agent.name);
}

enum AgentLoopState {
    Next(ResumePoint),
    Done(ResumePoint),
}

/// What the agent turn produced, distinguishing suspension from normal
/// completion so the outer loop knows whether to preserve `active_thread`.
enum TurnOutcome {
    /// The turn completed normally; the agent is idle.
    Completed,
    /// The agent suspended for approval. The outer loop moves `active_thread`
    /// into `suspended_thread` and waits for a Resume envelope so that
    /// `session.resume()` can deliver the decision in-process. On Exit,
    /// `suspended_thread` is restored into `active_thread` so the snapshot
    /// records the pending thread_id for restart-based resume.
    Suspended,
    /// The exit barrier was already set when the turn started or checked.
    ExitAcquired,
}

struct AgentLoop<'a, C: LLMProvider + Clone> {
    runtime: AgentRuntime,
    agent: &'a mut Agent,
    cancel: CancellationToken,
    config: RunConfig<C>,
    thread_id: ThreadId,
    reply_target: Option<ReplyTarget>,
}

impl<'a, C: LLMProvider + Clone> AgentLoop<'a, C> {
    async fn run(&mut self, envelope: Option<Envelope>) -> Result<TurnOutcome, String> {
        // Load stored checkpoint and scatter its fields into the appropriate
        // locations. After this block the stored type is gone — only the
        // `resume_point` local variable carries forward.
        let stored = self
            .runtime
            .session_storage
            .load_checkpoint(self.thread_id.as_ref())
            .await?;
        let (mut resume_point, mut suspended_at): (ResumePoint, jiff::Timestamp) =
            if let Some(stored) = stored {
                self.agent
                    .restore_history(stored.messages, stored.todos)
                    .await;
                self.reply_target = stored.reply_target;
                (stored.resume_point.into(), stored.suspended_at)
            } else {
                // The Agent instance may be reused across different thread IDs
                // (e.g. stateless subagent calls), so we must clear any stale
                // in-memory state to avoid leaking conversation across threads.
                self.agent.restore_history(vec![], vec![]).await;
                self.reply_target = None;
                (ResumePoint::Generation, jiff::Timestamp::default())
            };

        if let Some(envelope) = envelope {
            match self.handle_envelope(resume_point, envelope).await {
                AgentLoopState::Next(rp) => resume_point = rp,
                AgentLoopState::Done(rp) => {
                    self.save_checkpoint(rp, suspended_at).await;
                    return Ok(TurnOutcome::Completed);
                }
            }
        }

        let mut exit_acquired = false;
        let mut suspended = false;
        loop {
            if self.runtime.exit_barrier.is_exiting() {
                exit_acquired = true;
                break;
            }
            let current = std::mem::take(&mut resume_point);
            match current {
                ResumePoint::Generation => match self.handle_generation().await {
                    AgentLoopState::Next(rp @ ResumePoint::PendingApproval { .. }) => {
                        suspended_at = jiff::Timestamp::now();
                        resume_point = rp;
                    }
                    AgentLoopState::Next(rp) => resume_point = rp,
                    AgentLoopState::Done(rp) => {
                        resume_point = rp;
                        break;
                    }
                },
                ResumePoint::ToolExecution(tool_execution_state) => {
                    match self.handle_tool_execution(tool_execution_state).await {
                        AgentLoopState::Next(rp @ ResumePoint::ToolExecution(_)) => {
                            resume_point = rp;
                            break;
                        }
                        AgentLoopState::Next(rp) => resume_point = rp,
                        AgentLoopState::Done(rp) => {
                            resume_point = rp;
                            break;
                        }
                    }
                }
                ResumePoint::PendingApproval {
                    pending_approval_calls,
                    pending_calls,
                } => {
                    let has_pending = !pending_approval_calls.is_empty();
                    let pending = PendingApproval {
                        thread_id: self.thread_id.as_ref().to_string(),
                        agent_name: self.agent.name.to_string(),
                        calls: pending_approval_calls.iter().cloned().collect(),
                        suspended_at,
                    };
                    resume_point = ResumePoint::PendingApproval {
                        pending_approval_calls,
                        pending_calls,
                    };
                    if has_pending {
                        suspended = true;
                        self.runtime
                            .emit_event(
                                self.agent.name.clone(),
                                self.thread_id.clone(),
                                AgentEvent::Suspended(pending),
                            )
                            .await;
                    }
                    break;
                }
            }
        }

        self.save_checkpoint(resume_point, suspended_at).await;
        Ok(if exit_acquired {
            TurnOutcome::ExitAcquired
        } else if suspended {
            TurnOutcome::Suspended
        } else {
            TurnOutcome::Completed
        })
    }

    async fn save_checkpoint(&self, resume_point: ResumePoint, suspended_at: jiff::Timestamp) {
        let stored = StoredCheckpoint {
            thread_id: self.thread_id.as_ref().to_string(),
            agent_name: self.agent.name.to_string(),
            reply_target: self.reply_target.clone(),
            messages: self.agent.history().await,
            todos: self.agent.todos().await,
            resume_point: resume_point.into(),
            suspended_at,
        };
        if let Err(err) = self
            .runtime
            .session_storage
            .save_checkpoint(self.thread_id.as_ref().to_string(), stored)
            .await
        {
            error!(
                "Failed to save checkpoint for thread {}: {}",
                self.thread_id.as_ref(),
                err
            );
        }
    }

    async fn handle_envelope(
        &mut self,
        resume_point: ResumePoint,
        envelope: Envelope,
    ) -> AgentLoopState {
        match resume_point {
            ResumePoint::Generation => {
                match &envelope.body {
                    EnvelopeBody::Task(task) => {
                        self.reply_target = None;
                        self.agent
                            .add_message(Message::User(UserMessage(task.clone())))
                            .await
                    }
                    EnvelopeBody::ToolCall { task, .. } => {
                        self.reply_target = reply_target_from_envelope(&envelope);
                        self.agent
                            .add_message(Message::User(UserMessage(task.clone())))
                            .await
                    }
                    _ => {
                        warn!("unexpected envelope {:?}", envelope);
                        return AgentLoopState::Done(ResumePoint::Generation);
                    }
                }
                AgentLoopState::Next(ResumePoint::Generation)
            }
            ResumePoint::ToolExecution(mut tool_execution) => {
                if !tool_execution.pending_replies.is_empty() {
                    match &envelope {
                        Envelope {
                            body: EnvelopeBody::Reply { call_id, output },
                            ..
                        } => {
                            if let Some(pos) = tool_execution
                                .pending_replies
                                .iter()
                                .position(|call| &call.call_id == call_id)
                            {
                                let tc = tool_execution.pending_replies.remove(pos);
                                let tool_message = ToolMessage {
                                    id: tc.call_id,
                                    name: tc.tool_name,
                                    output: output.clone(),
                                    outcome: tc.outcome,
                                };
                                self.agent
                                    .add_message(Message::Tool(tool_message.clone()))
                                    .await;
                                self.runtime
                                    .emit_event(
                                        self.agent.name.clone(),
                                        self.thread_id.clone(),
                                        AgentEvent::ToolCallEnd(tool_message),
                                    )
                                    .await;
                            }
                        }
                        Envelope {
                            body: EnvelopeBody::Task(task) | EnvelopeBody::ToolCall { task, .. },
                            ..
                        } => {
                            // A new task/tool-call arrived while waiting for subagent replies
                            // (stale state after abort or process restart). Write aborted
                            // ToolMessages to keep history valid, then start fresh.
                            warn!(
                                "Received envelope while waiting for {} subagent reply/replies; marking as aborted",
                                tool_execution.pending_replies.len()
                            );
                            for pending in tool_execution.pending_replies.drain(..) {
                                self.agent
                                    .add_message(Message::Tool(ToolMessage {
                                        id: pending.call_id,
                                        name: pending.tool_name,
                                        output: ToolOutput::Err(
                                            "Tool execution was interrupted by the user"
                                                .to_string(),
                                        ),
                                        outcome: ToolCallOutcome::Aborted,
                                    }))
                                    .await;
                            }
                            self.reply_target = reply_target_from_envelope(&envelope);
                            self.agent
                                .add_message(Message::User(UserMessage(task.clone())))
                                .await;
                            return AgentLoopState::Next(ResumePoint::Generation);
                        }
                        _ => {
                            warn!("expect a reply envelope but got a {:?}", envelope);
                            return AgentLoopState::Done(ResumePoint::ToolExecution(
                                tool_execution,
                            ));
                        }
                    }
                }
                if tool_execution.pending_replies.is_empty() {
                    return AgentLoopState::Next(ResumePoint::Generation);
                }
                AgentLoopState::Next(ResumePoint::ToolExecution(tool_execution))
            }
            ResumePoint::PendingApproval {
                mut pending_approval_calls,
                mut pending_calls,
            } => {
                match &envelope.body {
                    EnvelopeBody::Task(task) | EnvelopeBody::ToolCall { task, .. } => {
                        // Stale PendingApproval state: new task or sub-agent re-invocation
                        // arrived (e.g. after abort). Write aborted ToolMessages for all
                        // pending calls so the history stays valid, then start fresh.
                        warn!(
                            "Received envelope while suspended for approval; discarding {} pending call(s)",
                            pending_approval_calls.len()
                        );
                        for tc in pending_approval_calls.drain(..) {
                            self.agent
                                .add_message(Message::Tool(ToolMessage {
                                    id: tc.id,
                                    name: tc.name,
                                    output: ToolOutput::Err(
                                        "Tool execution was interrupted by the user".to_string(),
                                    ),
                                    outcome: ToolCallOutcome::Aborted,
                                }))
                                .await;
                        }
                        for tc in pending_calls.drain(..) {
                            self.agent
                                .add_message(Message::Tool(ToolMessage {
                                    id: tc.tool_call.id,
                                    name: tc.tool_call.name,
                                    output: ToolOutput::Err(
                                        "Tool execution was interrupted by the user".to_string(),
                                    ),
                                    outcome: ToolCallOutcome::Aborted,
                                }))
                                .await;
                        }
                        self.reply_target = reply_target_from_envelope(&envelope);
                        self.agent
                            .add_message(Message::User(UserMessage(task.clone())))
                            .await;
                        AgentLoopState::Next(ResumePoint::Generation)
                    }
                    EnvelopeBody::Resume(decision) => {
                        let resolution_map: HashMap<String, ToolCallResolution> =
                            decision.resolutions.iter().cloned().collect();
                        for tc in pending_approval_calls.drain(..) {
                            let resolution = resolution_map
                                .get(&tc.id)
                                .cloned()
                                .unwrap_or(ToolCallResolution::Rejected { reason: None });
                            match resolution {
                                ToolCallResolution::Execute => {
                                    pending_calls.push_back(PendingToolCall {
                                        tool_call: tc,
                                        outcome: ToolCallOutcome::Approved,
                                    });
                                }
                                ToolCallResolution::Resolved(output) => {
                                    let tool_message = ToolMessage {
                                        id: tc.id,
                                        name: tc.name,
                                        output,
                                        outcome: ToolCallOutcome::Resolved,
                                    };
                                    self.agent
                                        .add_message(Message::Tool(tool_message.clone()))
                                        .await;
                                    self.runtime
                                        .emit_event(
                                            self.agent.name.clone(),
                                            self.thread_id.clone(),
                                            AgentEvent::ToolCallEnd(tool_message),
                                        )
                                        .await;
                                }
                                ToolCallResolution::Rejected { reason } => {
                                    let tool_message = ToolMessage {
                                        id: tc.id,
                                        name: tc.name,
                                        output: ToolOutput::Err(
                                            reason
                                                .clone()
                                                .unwrap_or_else(|| "Rejected by user".to_string()),
                                        ),
                                        outcome: ToolCallOutcome::Rejected { reason },
                                    };
                                    self.agent
                                        .add_message(Message::Tool(tool_message.clone()))
                                        .await;
                                    self.runtime
                                        .emit_event(
                                            self.agent.name.clone(),
                                            self.thread_id.clone(),
                                            AgentEvent::ToolCallEnd(tool_message),
                                        )
                                        .await;
                                }
                            }
                        }
                        AgentLoopState::Next(ResumePoint::ToolExecution(ToolExecutionState {
                            pending_replies: vec![],
                            tool_calls: pending_calls.clone(),
                        }))
                    }
                    _ => {
                        warn!(
                            "unexpected envelope while suspended for approval: {:?}",
                            envelope
                        );
                        AgentLoopState::Done(ResumePoint::PendingApproval {
                            pending_approval_calls,
                            pending_calls,
                        })
                    }
                }
            }
        }
    }

    async fn handle_generation(&mut self) -> AgentLoopState {
        let thread_id = self.thread_id.clone();
        let request = ChatCompletionRequest {
            model: self.config.model.clone(),
            max_completion_tokens: self.config.max_completion_tokens,
            temperature: self.config.temperature,
            reasoning_effort: self.config.reasoning_effort,
            messages: self.agent.messages().await,
            tools: {
                let mut tools = self.agent.tools.descriptors();
                tools.extend(self.agent.subagents.descriptors());
                tools
            },
        };
        self.runtime
            .emit_event(
                self.agent.name.clone(),
                thread_id.clone(),
                AgentEvent::LLMStart(request.clone()),
            )
            .await;

        let mut llm_stream = std::pin::pin!(self.config.provider.stream(request));
        let mut partial_content = String::new();
        let llm_result = loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    self.runtime.emit_event(self.agent.name.clone(), thread_id.clone(), AgentEvent::Aborted(AbortedTarget::Generation)).await;
                    if !partial_content.is_empty() {
                        self.agent.add_message(Message::Assistant(coda_core::llm::AssistantMessage {
                            content: partial_content + "\n[Generation was interrupted by the user]",
                            aborted: true,
                            ..Default::default()
                        })).await;
                    }
                    // TODO: returning Err here may cause a downstream Error event in addition to Aborted(Generation). Consider a distinct abort result.
                    break Err("Aborted by user".to_string());
                }
                event = llm_stream.next() => {
                    match event {
                        Some(Ok(LLMStreamEvent::ContentChunk(chunk))) => {
                            partial_content.push_str(&chunk);
                            self.runtime.emit_event(self.agent.name.clone(), thread_id.clone(),AgentEvent::LLMContentChunk(chunk)).await;
                        }
                        Some(Ok(LLMStreamEvent::ReasoningChunk(chunk))) => {
                            // Assistant content and reasoning remain separate.
                            // The provider retains reasoning needed for later tool turns.
                            self.runtime.emit_event(self.agent.name.clone(), thread_id.clone(),AgentEvent::LLMReasoningChunk(chunk)).await;
                        }
                        Some(Ok(LLMStreamEvent::Completed(message))) => break Ok(message),
                        Some(Err(err)) => break Err(err.to_string()),
                        None => break Err(StreamError::InvalidResponse(
                            "LLM stream ended without Completed event".to_string(),
                        )
                        .to_string()),
                    }
                }
            }
        };
        match llm_result {
            Ok(assistant_message) => {
                self.agent
                    .add_message(Message::Assistant(assistant_message.clone()))
                    .await;
                self.runtime
                    .emit_event(
                        self.agent.name.clone(),
                        thread_id.clone(),
                        AgentEvent::LLMEnd(assistant_message.clone()),
                    )
                    .await;

                if assistant_message.tool_calls.is_empty() {
                    if let Some(reply_target) = self.reply_target.take() {
                        let err = self
                            .runtime
                            .send_message(Envelope::with_id(|id| Envelope {
                                id,
                                from: Sender::Agent {
                                    name: self.agent.name.clone(),
                                    thread_id: thread_id.clone(),
                                },
                                to: Receiver {
                                    name: reply_target.sender_name.clone(),
                                    thread_id: ThreadId::from(
                                        reply_target.sender_thread_id.clone(),
                                    ),
                                },
                                reply_to: Some(reply_target.envelope_id.clone()),
                                body: EnvelopeBody::Reply {
                                    call_id: reply_target.call_id.clone(),
                                    output: ToolOutput::Ok(assistant_message.content.clone()),
                                },
                            }))
                            .await;
                        if let Err(err) = err {
                            error!("Failed to send LLM reply: {}", err);
                        }
                    }
                    AgentLoopState::Done(ResumePoint::Generation)
                } else {
                    let (pending_approval_calls, auto_calls) = {
                        match &self.config.tool_approval {
                            ToolApprovalMode::Auto => (vec![], assistant_message.tool_calls),
                            ToolApprovalMode::Manual => (assistant_message.tool_calls, vec![]),
                            ToolApprovalMode::RequireWhen(predicate) => assistant_message
                                .tool_calls
                                .into_iter()
                                .partition(|call| predicate(call)),
                        }
                    };
                    if !pending_approval_calls.is_empty() {
                        AgentLoopState::Next(ResumePoint::PendingApproval {
                            pending_approval_calls: pending_approval_calls.into(),
                            pending_calls: auto_calls
                                .into_iter()
                                .map(|call| PendingToolCall {
                                    tool_call: call,
                                    outcome: ToolCallOutcome::Auto,
                                })
                                .collect(),
                        })
                    } else {
                        AgentLoopState::Next(ResumePoint::ToolExecution(ToolExecutionState {
                            pending_replies: vec![],
                            tool_calls: auto_calls
                                .into_iter()
                                .map(|call| PendingToolCall {
                                    tool_call: call,
                                    outcome: ToolCallOutcome::Auto,
                                })
                                .collect(),
                        }))
                    }
                }
            }
            Err(err) => {
                error!("LLM generation error: {}", err);
                if let Some(reply_target) = self.reply_target.take() {
                    let ret = self
                        .runtime
                        .send_message(Envelope::with_id(|id| Envelope {
                            id,
                            from: Sender::Agent {
                                name: self.agent.name.clone(),
                                thread_id,
                            },
                            to: Receiver {
                                name: reply_target.sender_name.clone(),
                                thread_id: ThreadId::from(reply_target.sender_thread_id.clone()),
                            },
                            reply_to: Some(reply_target.envelope_id.clone()),
                            body: EnvelopeBody::Reply {
                                call_id: reply_target.call_id.clone(),
                                output: ToolOutput::Err(err),
                            },
                        }))
                        .await;
                    if let Err(err) = ret {
                        error!("Failed to send LLM error reply: {}", err);
                    }
                } else {
                    self.runtime
                        .emit_event(
                            self.agent.name.clone(),
                            self.thread_id.clone(),
                            AgentEvent::Error(err),
                        )
                        .await;
                }
                AgentLoopState::Done(ResumePoint::Generation)
            }
        }
    }

    async fn handle_tool_execution(
        &mut self,
        mut tool_execution: ToolExecutionState,
    ) -> AgentLoopState {
        let concurrent_stateful =
            concurrent_stateful_subagents(self.agent, &tool_execution.tool_calls);
        // Tracks local tool calls that have not yet completed, keyed by tool call id.
        let mut pending_local: HashMap<String, PendingToolCall> = HashMap::new();
        let mut futures = futures::stream::FuturesUnordered::new();
        for tc in &tool_execution.tool_calls {
            if let Some(subagent) = self.agent.subagents.get(&tc.tool_call.name) {
                if subagent.mode == SubAgentMode::Stateful
                    && concurrent_stateful.contains(&tc.tool_call.name)
                {
                    // reject concurrent calls to stateful subagent
                    self.agent
                    .add_message(Message::Tool(ToolMessage {
                        id: tc.tool_call.id.clone(),
                        name: tc.tool_call.name.clone(),
                        output: ToolOutput::Err(format!("Concurrent invocation of sub-agent '{}' is not allowed. Call it sequentially.", tc.tool_call.name)),
                        outcome: tc.outcome.clone()
                    })).await;
                    continue;
                }

                let subagent_thread_id = if subagent.mode == SubAgentMode::Stateless {
                    // Stateless: derive thread id from the parent thread + call id so each
                    // invocation gets an independent session. The parent thread_id is a
                    // valid UUID, so the uuid5 derivation never falls back to nil.
                    ThreadId::from_uuid5(&self.thread_id, &tc.tool_call.id)
                } else {
                    // Stateful: stable thread id derived from the parent thread so the
                    // sub-agent session persists across calls within the same conversation.
                    ThreadId::from_uuid5(&self.thread_id, &tc.tool_call.name)
                };
                let subagent_tool_call_envelope = Envelope::with_id(|id| Envelope {
                    id,
                    from: Sender::Agent {
                        name: self.agent.name.clone(),
                        thread_id: self.thread_id.clone(),
                    },
                    to: Receiver {
                        name: tc.tool_call.name.clone(),
                        thread_id: subagent_thread_id,
                    },
                    reply_to: None,
                    body: EnvelopeBody::ToolCall {
                        call_id: tc.tool_call.id.clone(),
                        // Sub-agent tools always take {"task": "..."} — extract the string.
                        task: serde_json::from_str::<serde_json::Value>(
                            tc.tool_call.arguments.as_deref().unwrap_or("{}"),
                        )
                        .ok()
                        .and_then(|v| v["task"].as_str().map(String::from))
                        .unwrap_or_default(),
                    },
                });
                let ret = self.runtime.send_message(subagent_tool_call_envelope).await;
                if let Err(err) = ret {
                    error!(
                        "Failed to send tool call to subagent {}, error: {}",
                        tc.tool_call.name, err
                    );
                    self.agent
                        .add_message(Message::Tool(ToolMessage {
                            id: tc.tool_call.id.clone(),
                            name: tc.tool_call.name.clone(),
                            output: ToolOutput::Err(format!(
                                "Failed to dispatch to subagent '{}': {}",
                                tc.tool_call.name, err
                            )),
                            outcome: tc.outcome.clone(),
                        }))
                        .await;
                } else {
                    self.runtime
                        .emit_event(
                            self.agent.name.clone(),
                            self.thread_id.clone(),
                            AgentEvent::ToolCallStart(tc.tool_call.clone()),
                        )
                        .await;
                    tool_execution.pending_replies.push(PendingReply {
                        call_id: tc.tool_call.id.clone(),
                        tool_name: tc.tool_call.name.clone(),
                        outcome: tc.outcome.clone(),
                    });
                }
            } else if let Some(tool) = self.agent.tools.get(&tc.tool_call.name) {
                self.runtime
                    .emit_event(
                        self.agent.name.clone(),
                        self.thread_id.clone(),
                        AgentEvent::ToolCallStart(tc.tool_call.clone()),
                    )
                    .await;
                pending_local.insert(tc.tool_call.id.clone(), tc.clone());
                let tc = tc.clone();
                let future = async move {
                    let output = tool
                        .execute(tc.tool_call.arguments.clone().unwrap_or_default())
                        .await;
                    (tc, output)
                };
                futures.push(future);
            } else {
                // No such tool
                self.agent
                    .add_message(Message::Tool(ToolMessage {
                        id: tc.tool_call.id.clone(),
                        name: tc.tool_call.name.clone(),
                        output: ToolOutput::Err(format!("No such tool: {}", tc.tool_call.name)),
                        outcome: tc.outcome.clone(),
                    }))
                    .await;
            }
        }
        // Remove pending replies from tool calls.
        tool_execution.tool_calls.retain(|x| {
            tool_execution
                .pending_replies
                .iter()
                .all(|y| x.tool_call.id != y.call_id)
        });

        let aborted = loop {
            if futures.is_empty() {
                // Even with no local futures, the cancel may have fired while dispatching
                // subagent calls; detect it here so we don't silently suspend on pending_replies.
                break self.cancel.is_cancelled();
            }
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break true,
                Some((tc, result)) = futures.next() => {
                    pending_local.remove(&tc.tool_call.id);
                    let message = ToolMessage {
                        id: tc.tool_call.id,
                        name: tc.tool_call.name,
                        output: match result {
                            Ok(output) => ToolOutput::Ok(output),
                            Err(err) => ToolOutput::Err(format!("Tool execution error: {}", err)),
                        },
                        outcome: tc.outcome,
                    };
                    self.agent.add_message(Message::Tool(message.clone())).await;
                    self.runtime.emit_event(
                        self.agent.name.clone(),
                        self.thread_id.clone(),
                        AgentEvent::ToolCallEnd(message),
                    ).await;
                }
            }
        };
        tool_execution.tool_calls.clear();

        if aborted {
            let mut aborted_ids: Vec<String> = pending_local.keys().cloned().collect();
            for (id, tc) in pending_local {
                self.agent
                    .add_message(Message::Tool(ToolMessage {
                        id,
                        name: tc.tool_call.name,
                        output: ToolOutput::Err(
                            "Tool execution was interrupted by the user".to_string(),
                        ),
                        outcome: ToolCallOutcome::Aborted,
                    }))
                    .await;
            }
            // Also write aborted ToolMessages for any pending subagent replies.
            for pending in tool_execution.pending_replies.drain(..) {
                aborted_ids.push(pending.call_id.clone());
                self.agent
                    .add_message(Message::Tool(ToolMessage {
                        id: pending.call_id,
                        name: pending.tool_name,
                        output: ToolOutput::Err(
                            "Tool execution was interrupted by the user".to_string(),
                        ),
                        outcome: ToolCallOutcome::Aborted,
                    }))
                    .await;
            }
            self.runtime
                .emit_event(
                    self.agent.name.clone(),
                    self.thread_id.clone(),
                    AgentEvent::Aborted(AbortedTarget::ToolCalls(aborted_ids)),
                )
                .await;
            return AgentLoopState::Done(ResumePoint::Generation);
        }

        if !tool_execution.pending_replies.is_empty() {
            AgentLoopState::Next(ResumePoint::ToolExecution(tool_execution.clone()))
        } else {
            AgentLoopState::Next(ResumePoint::Generation)
        }
    }
}

fn reply_target_from_envelope(envelope: &Envelope) -> Option<ReplyTarget> {
    match (&envelope.from, &envelope.body) {
        (Sender::Agent { name, thread_id }, EnvelopeBody::ToolCall { call_id, .. }) => {
            Some(ReplyTarget {
                envelope_id: envelope.id.clone(),
                sender_name: name.clone(),
                sender_thread_id: thread_id.as_ref().to_string(),
                call_id: call_id.clone(),
            })
        }
        _ => None,
    }
}

fn concurrent_stateful_subagents(
    agent: &Agent,
    tool_calls: &VecDeque<PendingToolCall>,
) -> HashSet<String> {
    let mut counts = std::collections::HashMap::new();
    for tc in tool_calls {
        if let Some(subagent) = agent.subagents.get(&tc.tool_call.name)
            && subagent.mode == crate::SubAgentMode::Stateful
        {
            *counts.entry(tc.tool_call.name.clone()).or_insert(0usize) += 1;
        }
    }
    counts
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name))
        .collect()
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
