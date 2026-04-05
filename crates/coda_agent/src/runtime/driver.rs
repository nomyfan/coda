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

use crate::{
    AbortedTarget, Agent, AgentCheckpoint, AgentEvent, Envelope, RunConfig, Sender, SubAgentMode,
    ThreadId, ToolApprovalMode, ToolCallResolution,
    agent::{
        EnvelopeBody, PendingReply, PendingToolCall, Receiver, ReplyTarget, ResumePoint,
        ToolExecutionState,
    },
    runtime::{AgentControl, AgentRuntime},
};

#[instrument(skip_all, fields(agent = %agent.name))]
pub(crate) async fn run_agent(
    runtime: AgentRuntime,
    mut agent: Agent,
    mut control_rx: mpsc::Receiver<AgentControl>,
    mut envelope_rx: mpsc::Receiver<Envelope>,
    config: RunConfig<impl LLMProvider + Clone>,
) {
    loop {
        // Wait for the next envelope, but allow Exit to break the loop.
        let envelope = tokio::select! {
            biased;
            cmd = control_rx.recv() => {
                if let Some(AgentControl::Exit) = cmd { break; }
                continue;
            }
            envelope = envelope_rx.recv() => match envelope {
                Some(e) => e,
                None => break,
            },
        };

        let cancel = CancellationToken::new();
        let mut agent_loop = AgentLoop {
            runtime: runtime.clone(),
            agent: &mut agent,
            cancel: cancel.clone(),
            config: config.clone(),
            thread_id: envelope.to.thread_id.clone(),
            envelope,
            reply_target: None,
        };
        let mut run_fut = std::pin::pin!(agent_loop.run());

        // Race the agent loop against incoming control signals.
        let should_exit = tokio::select! {
            biased;
            cmd = control_rx.recv() => {
                // Signal cancellation and wait for the loop to clean up
                // (emit events, save checkpoint) before proceeding.
                cancel.cancel();
                if let Err(err) = (&mut run_fut).await {
                    error!("Error in agent loop: {}", err);
                }
                matches!(cmd, Some(AgentControl::Exit))
            }
            ret = &mut run_fut => {
                if let Err(err) = ret {
                    error!("Error in agent loop: {}", err);
                }
                false
            }
        };

        if should_exit {
            break;
        }
    }
    info!("Agent {} exiting", agent.name);
}

enum AgentLoopState {
    Next(ResumePoint),
    Done,
}

struct AgentLoop<'a, C: LLMProvider + Clone> {
    runtime: AgentRuntime,
    agent: &'a mut Agent,
    cancel: CancellationToken,
    config: RunConfig<C>,
    thread_id: ThreadId,
    envelope: Envelope,
    reply_target: Option<ReplyTarget>,
}

impl<'a, C: LLMProvider + Clone> AgentLoop<'a, C> {
    async fn run(&mut self) -> Result<(), String> {
        let mut checkpoint = self
            .runtime
            .session_storage
            .load_checkpoint(self.thread_id.as_ref())
            .await?
            .unwrap_or_else(|| AgentCheckpoint {
                thread_id: self.thread_id.as_ref().to_string(),
                agent_name: self.agent.name.to_string(),
                ..Default::default()
            });
        self.agent
            .restore_history(checkpoint.messages.clone(), checkpoint.todos.clone())
            .await;
        self.reply_target = checkpoint.reply_target.clone();
        match self.handle_envelope(&mut checkpoint).await {
            AgentLoopState::Next(resume_point) => checkpoint.resume_point = resume_point,
            AgentLoopState::Done => {
                self.save_checkpoint(checkpoint).await;
                return Ok(());
            }
        }

        loop {
            match &checkpoint.resume_point {
                ResumePoint::Generation => match self.handle_generation().await {
                    AgentLoopState::Next(resume_point) => checkpoint.resume_point = resume_point,
                    AgentLoopState::Done => break,
                },
                ResumePoint::ToolExecution(tool_execution_state) => match self
                    .handle_tool_execution(tool_execution_state.clone())
                    .await
                {
                    AgentLoopState::Next(resume_point @ ResumePoint::ToolExecution(_)) => {
                        checkpoint.resume_point = resume_point;
                        break;
                    }
                    AgentLoopState::Next(resume_point) => checkpoint.resume_point = resume_point,
                    AgentLoopState::Done => {
                        // Aborted; reset so no stale ToolExecution state is persisted.
                        checkpoint.resume_point = ResumePoint::Generation;
                        break;
                    }
                },
                ResumePoint::PendingApproval {
                    pending_approval_calls,
                    ..
                } => {
                    if !pending_approval_calls.is_empty() {
                        checkpoint.todos = self.agent.todos().await;
                        checkpoint.messages = self.agent.history().await;
                        self.runtime.emit_event(
                            self.agent.name.clone(),
                            self.thread_id.clone(),
                            AgentEvent::Suspended(checkpoint.clone()),
                        );
                    }
                    break;
                }
            }
        }

        self.save_checkpoint(checkpoint).await;
        Ok(())
    }

    async fn save_checkpoint(&self, mut checkpoint: AgentCheckpoint) {
        checkpoint.todos = self.agent.todos().await;
        checkpoint.messages = self.agent.history().await;
        checkpoint.reply_target = self.reply_target.clone();
        checkpoint.suspended_at = jiff::Timestamp::now();
        if let Err(err) = self
            .runtime
            .session_storage
            .save_checkpoint(self.thread_id.as_ref().to_string(), checkpoint)
            .await
        {
            error!(
                "Failed to save checkpoint for thread {}: {}",
                self.thread_id.as_ref(),
                err
            );
        }
    }

    async fn handle_envelope(&mut self, checkpoint: &mut AgentCheckpoint) -> AgentLoopState {
        match &mut checkpoint.resume_point {
            ResumePoint::Generation => {
                match &self.envelope.body {
                    EnvelopeBody::Task(task) => {
                        self.reply_target = None;
                        self.agent
                            .add_message(Message::User(UserMessage(task.clone())))
                            .await
                    }
                    EnvelopeBody::ToolCall { task, .. } => {
                        self.reply_target = reply_target_from_envelope(&self.envelope);
                        self.agent
                            .add_message(Message::User(UserMessage(task.clone())))
                            .await
                    }
                    _ => {
                        warn!("unexpected envelope {:?}", self.envelope);
                        return AgentLoopState::Done;
                    }
                }
                return AgentLoopState::Next(ResumePoint::Generation);
            }
            ResumePoint::ToolExecution(tool_execution) => {
                if !tool_execution.pending_replies.is_empty() {
                    match &self.envelope {
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
                                self.runtime.emit_event(
                                    self.agent.name.clone(),
                                    self.thread_id.clone(),
                                    AgentEvent::ToolCallEnd(tool_message),
                                );
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
                            self.reply_target = reply_target_from_envelope(&self.envelope);
                            self.agent
                                .add_message(Message::User(UserMessage(task.clone())))
                                .await;
                            return AgentLoopState::Next(ResumePoint::Generation);
                        }
                        _ => {
                            warn!("expect a reply envelope but got a {:?}", self.envelope);
                            return AgentLoopState::Done;
                        }
                    }
                }
                if tool_execution.pending_replies.is_empty() {
                    return AgentLoopState::Next(ResumePoint::Generation);
                }
                return AgentLoopState::Next(ResumePoint::ToolExecution(tool_execution.clone()));
            }
            ResumePoint::PendingApproval {
                pending_approval_calls,
                pending_calls,
            } => {
                match &self.envelope.body {
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
                        self.reply_target = reply_target_from_envelope(&self.envelope);
                        self.agent
                            .add_message(Message::User(UserMessage(task.clone())))
                            .await;
                        return AgentLoopState::Next(ResumePoint::Generation);
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
                                    self.agent
                                        .add_message(Message::Tool(ToolMessage {
                                            id: tc.id,
                                            name: tc.name,
                                            output,
                                            outcome: ToolCallOutcome::Resolved,
                                        }))
                                        .await;
                                }
                                ToolCallResolution::Rejected { reason } => {
                                    self.agent
                                        .add_message(Message::Tool(ToolMessage {
                                            id: tc.id,
                                            name: tc.name,
                                            output: ToolOutput::Err(
                                                reason.clone().unwrap_or_else(|| {
                                                    "Rejected by user".to_string()
                                                }),
                                            ),
                                            outcome: ToolCallOutcome::Rejected { reason },
                                        }))
                                        .await;
                                }
                            }
                        }
                        return AgentLoopState::Next(ResumePoint::ToolExecution(
                            ToolExecutionState {
                                pending_replies: vec![],
                                tool_calls: pending_calls.clone(),
                            },
                        ));
                    }
                    _ => {
                        warn!(
                            "unexpected envelope while suspended for approval: {:?}",
                            self.envelope
                        );
                        return AgentLoopState::Done;
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
            messages: self.agent.messages().await,
            tools: {
                let mut tools = self.agent.tools.descriptors();
                tools.extend(self.agent.subagents.descriptors());
                tools
            },
        };
        self.runtime.emit_event(
            self.agent.name.clone(),
            thread_id.clone(),
            AgentEvent::LLMStart(request.clone()),
        );

        let mut llm_stream = std::pin::pin!(self.config.provider.stream(request));
        let mut partial_content = String::new();
        let llm_result = loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    self.runtime.emit_event(self.agent.name.clone(), thread_id.clone(), AgentEvent::Aborted(AbortedTarget::Generation));
                    if !partial_content.is_empty() {
                        self.agent.add_message(Message::Assistant(coda_core::llm::AssistantMessage {
                            content: partial_content + "\n[Generation was interrupted by the user]",
                            aborted: true,
                            ..Default::default()
                        })).await;
                    }
                    break Err("Aborted by user".to_string());
                }
                event = llm_stream.next() => {
                    match event {
                        Some(Ok(LLMStreamEvent::ContentChunk(chunk))) => {
                            partial_content.push_str(&chunk);
                            self.runtime.emit_event(self.agent.name.clone(), thread_id.clone(),AgentEvent::LLMContentChunk(chunk));
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
                self.runtime.emit_event(
                    self.agent.name.clone(),
                    thread_id.clone(),
                    AgentEvent::LLMEnd(assistant_message.clone()),
                );

                if assistant_message.tool_calls.is_empty() {
                    if let Some(reply_target) = self.reply_target.take() {
                        let err = self
                            .runtime
                            .send_message(Envelope::new(|id| Envelope {
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
                    return AgentLoopState::Done;
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
                        return AgentLoopState::Next(ResumePoint::PendingApproval {
                            pending_approval_calls: pending_approval_calls.into(),
                            pending_calls: auto_calls
                                .into_iter()
                                .map(|call| PendingToolCall {
                                    tool_call: call,
                                    outcome: ToolCallOutcome::Auto,
                                })
                                .collect(),
                        });
                    } else {
                        return AgentLoopState::Next(ResumePoint::ToolExecution(
                            ToolExecutionState {
                                pending_replies: vec![],
                                tool_calls: auto_calls
                                    .into_iter()
                                    .map(|call| PendingToolCall {
                                        tool_call: call,
                                        outcome: ToolCallOutcome::Auto,
                                    })
                                    .collect(),
                            },
                        ));
                    }
                }
            }
            Err(err) => {
                error!("LLM generation error: {}", err);
                if let Some(reply_target) = self.reply_target.take() {
                    let ret = self
                        .runtime
                        .send_message(Envelope::new(|id| Envelope {
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
                    self.runtime.emit_event(
                        self.agent.name.clone(),
                        self.thread_id.clone(),
                        AgentEvent::Error(err),
                    );
                }
                return AgentLoopState::Done;
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
                let subagent_tool_call_envelope = Envelope::new(|id| Envelope {
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
                    self.runtime.emit_event(
                        self.agent.name.clone(),
                        self.thread_id.clone(),
                        AgentEvent::ToolCallStart(tc.tool_call.clone()),
                    );
                    tool_execution.pending_replies.push(PendingReply {
                        call_id: tc.tool_call.id.clone(),
                        tool_name: tc.tool_call.name.clone(),
                        outcome: tc.outcome.clone(),
                    });
                }
            } else if let Some(tool) = self.agent.tools.get(&tc.tool_call.name) {
                self.runtime.emit_event(
                    self.agent.name.clone(),
                    self.thread_id.clone(),
                    AgentEvent::ToolCallStart(tc.tool_call.clone()),
                );
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
                    );
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
            self.runtime.emit_event(
                self.agent.name.clone(),
                self.thread_id.clone(),
                AgentEvent::Aborted(AbortedTarget::ToolCalls(aborted_ids)),
            );
            return AgentLoopState::Done;
        }

        if !tool_execution.pending_replies.is_empty() {
            return AgentLoopState::Next(ResumePoint::ToolExecution(tool_execution.clone()));
        } else {
            return AgentLoopState::Next(ResumePoint::Generation);
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
