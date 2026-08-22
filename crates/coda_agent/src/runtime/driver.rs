use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    vec,
};

use serde_json::Value;

use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, Message, MessageId,
    MessageOrigin, StreamError, ToolCall, ToolCallOutcome, ToolDefinition, ToolMessage, ToolOutput,
    TurnId, UserMessage,
};
use coda_core::tool::{
    HostToolCallError, HostToolCallResult, HostToolInvoker, ThreadState, ToolCallContext,
    ToolError, ToolResult, Tools,
};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, error, info, info_span, instrument, warn};

use super::AgentControl;
use crate::{
    AbortedTarget, Agent, AgentEvent, Envelope, PendingApproval, ResumeDecision, Sender,
    SubAgentMode, ThreadId, ToolApprovalMode, ToolCallResolution,
    agent::{
        AgentRunConfig, EnvelopeBody, PendingReply, PendingToolCall, PreparedToolCall, Receiver,
        ReplyTarget, ResumePoint, ToolExecutionMetadata, ToolExecutionState,
    },
    compaction, message_view,
    persist::StoredCheckpoint,
    runtime::AgentRuntime,
};

/// How long an aborted turn waits for in-flight tool calls to observe their
/// cancellation token, tear down their work (e.g. kill child processes), and
/// settle with partial output before their futures are dropped.
/// Shortened under `cfg(test)` so tests exercising the timeout path stay fast.
#[cfg(not(test))]
const TOOL_ABORT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const TOOL_ABORT_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long an auto-compaction's summarization call waits on its provider —
/// mirrors `SUMMARY_TIMEOUT` in the manual path. Unlike the main generation
/// call this step is silent, so without a bound a hung provider would stall
/// the turn with no visible cue to cancel it.
const AUTO_COMPACT_SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How long the root waits for a cancelled turn's sub-agents to answer before
/// giving up on them.
///
/// Only the wind-up wait is capped. A turn that is running normally may sit on
/// a sub-agent for as long as that sub-agent needs; a turn that has been asked
/// to stop should be answered within a few rounds of teardown, so silence past
/// this point means something is wedged rather than busy.
#[cfg(not(test))]
const WIND_UP_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(test)]
const WIND_UP_LIMIT: std::time::Duration = std::time::Duration::from_millis(400);

#[instrument(skip_all, fields(agent = %agent.name))]
pub(crate) async fn run_agent(
    runtime: AgentRuntime,
    active: (Option<ThreadId>, Option<ResumeDecision>),
    mut agent: Agent,
    mut control_rx: mpsc::Receiver<AgentControl>,
    mut envelope_rx: mpsc::Receiver<Envelope>,
    config: AgentRunConfig<impl LLMProvider + Clone>,
) {
    info!(
        "Agent {} is running (model: {})",
        agent.name, config.profile.label
    );
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
    // outer loop waits for a Resume envelope. But we
    // still need the thread_id available if Exit fires during that wait so the
    // snapshot can record the pending thread for restart-based resume.
    let mut suspended_thread: Option<ThreadId> = None;
    // Envelopes this agent refused because a turn was still winding up. They
    // keep their arrival order and go back in once it has.
    let mut deferred: VecDeque<Envelope> = VecDeque::new();
    // The thread parked waiting on sub-agent answers, and the turn it is
    // waiting for. While one is set, only what arrives on the wire may reach
    // this agent: replaying a held envelope would walk straight back into the
    // case that deferred it.
    let mut awaiting_replies: Option<(ThreadId, TurnId)> = None;
    loop {
        // First: if we have a queued resume envelope, run with it.
        // Otherwise: if there's an active thread to continue, just run it without waiting for a new envelope.
        let (thread_id, envelope) = if let Some(envelope) = pending_resume_envelope.take() {
            (envelope.to.thread_id.clone(), Some(envelope))
        } else if let Some(active_thread) = active_thread.take() {
            (active_thread, None)
        } else if let Some(envelope) = awaiting_replies
            .is_none()
            .then(|| deferred.pop_front())
            .flatten()
        {
            (envelope.to.thread_id.clone(), Some(envelope))
        } else {
            // A cancelled turn should be answered within a few rounds of
            // teardown. Past the limit the sub-agent is wedged, and the root
            // says so rather than waiting on it — but it says *unwritten*, not
            // *finished*: a turn whose content never landed has not ended,
            // whatever is on screen.
            let wind_up_limit = awaiting_replies
                .as_ref()
                .filter(|(thread_id, turn)| {
                    runtime.is_root_thread(thread_id) && runtime.turn_gate.is_cancelled(*turn)
                })
                .is_some();

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
                        Some(AgentControl::Abort) => {
                            // Nothing is running to cancel, but this agent may
                            // be parked on an approval for the very turn being
                            // stopped — and no envelope is coming to wake it,
                            // since the user answered with an abort instead of
                            // a decision. Drive it back in so it can wind up.
                            if let Some(parked) = suspended_thread.take() {
                                if runtime.thread_turn_cancelled(&parked).await {
                                    active_thread = Some(parked);
                                } else {
                                    suspended_thread = Some(parked);
                                }
                            }
                            continue;
                        }
                    }
                }
                envelope = envelope_rx.recv() => match envelope {
                    Some(e) => {
                        suspended_thread = None;
                        e
                    }
                    None => break,
                },
                _ = tokio::time::sleep(WIND_UP_LIMIT), if wind_up_limit => {
                    let (thread_id, turn) = awaiting_replies.take().expect("a limit implies a wait");
                    warn!(
                        "{} gave up waiting for a cancelled turn's sub-agents to answer",
                        agent.name
                    );
                    runtime
                        .emit_event(
                            agent.name.clone(),
                            thread_id,
                            turn,
                            AgentEvent::PersistFailed(
                                "sub-agents did not finish saving after the turn was stopped"
                                    .to_string(),
                            ),
                        )
                        .await;
                    continue;
                }
            };

            (next_envelope.to.thread_id.clone(), Some(next_envelope))
        };

        let cancel = CancellationToken::new();
        active_thread = Some(thread_id.clone());
        let turn = runtime
            .turn_gate
            .active_id()
            .unwrap_or_else(|| TurnId::from(MessageId::new()));
        let mut agent_loop = AgentLoop {
            runtime: runtime.clone(),
            agent: &mut agent,
            cancel: cancel.clone(),
            config: config.clone(),
            thread_id: thread_id.clone(),
            turn,
            reply_target: None,
            origin_thread: None,
        };
        let mut run_fut = std::pin::pin!(agent_loop.run(envelope));

        // Race the agent loop against incoming control signals.
        let should_exit = tokio::select! {
            biased;
            cmd = control_rx.recv() => {
                let mut should_exit = false;
                let mut cmd = cmd;
                // `Exit` is followed by an `Abort` once the caller runs out of
                // patience, and only that abort ends a turn `Exit` cannot
                // interrupt. So keep taking signals until one cancels the run:
                // reading a single one could spend it on a repeat `Exit`.
                let ret = loop {
                    let cancelled = match cmd {
                        Some(AgentControl::Abort) | None => {
                            cancel.cancel();
                            true
                        }
                        Some(AgentControl::Exit) => {
                            // Wait the agent loop to exit gracefully.
                            should_exit = true;
                            false
                        }
                    };
                    if cancelled {
                        break (&mut run_fut).await;
                    }
                    tokio::select! {
                        biased;
                        next = control_rx.recv() => cmd = next,
                        ret = &mut run_fut => break ret,
                    }
                };
                match ret {
                    Ok(TurnOutcome::ExitAcquired | TurnOutcome::Completed) => {
                        active_thread = None;
                        awaiting_replies = None;
                    }
                    Ok(TurnOutcome::AwaitingReplies(turn)) => {
                        active_thread = None;
                        awaiting_replies = Some((thread_id.clone(), turn));
                    }
                    Ok(TurnOutcome::Deferred { envelope, awaiting }) => {
                        active_thread = None;
                        // The queue only holds the envelope while this thread is
                        // known to be waiting, so the wait has to be recorded
                        // before it goes back.
                        awaiting_replies = awaiting.map(|turn| (thread_id.clone(), turn));
                        deferred.push_back(*envelope);
                    }
                    Ok(TurnOutcome::Suspended) => {
                        // Losing this thread loses the approval: a sub-agent's is
                        // found again only through the snapshot. Exiting keeps it
                        // in `active_thread` for the snapshot about to be taken.
                        // Otherwise it parks — unless the abort was the user
                        // taking the turn back, which nothing but a wind-up ends
                        // and no envelope is coming to prompt.
                        if !should_exit && !runtime.thread_turn_cancelled(&thread_id).await {
                            suspended_thread = active_thread.take();
                        }
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
                        awaiting_replies = None;
                    }
                    Ok(TurnOutcome::AwaitingReplies(turn)) => {
                        active_thread = None;
                        awaiting_replies = Some((thread_id.clone(), turn));
                    }
                    Ok(TurnOutcome::Deferred { envelope, awaiting }) => {
                        active_thread = None;
                        // The queue only holds the envelope while this thread is
                        // known to be waiting, so the wait has to be recorded
                        // before it goes back.
                        awaiting_replies = awaiting.map(|turn| (thread_id.clone(), turn));
                        deferred.push_back(*envelope);
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
    let mut envelopes: Vec<Envelope> = deferred.into();
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
    Done(ResumePoint, Box<TurnEnd>),
}

struct AgentToolInvoker {
    tools: Tools,
    approval: ToolApprovalMode,
    exposed_tools: Arc<[String]>,
    exposed_set: HashSet<String>,
}

impl AgentToolInvoker {
    fn new(tools: Tools, approval: ToolApprovalMode, exposed_tools: Vec<String>) -> Self {
        // Checkpoint data is not an authority source. Even if it is malformed
        // or manually altered, canonicalize it back to the unique, fixed-order
        // intersection of the MVP capability family and the current registry.
        let requested: HashSet<_> = exposed_tools.into_iter().collect();
        let exposed_tools: Vec<_> = coda_tools::PROGRAMMATIC_TOOL_NAMES
            .iter()
            .filter(|name| requested.contains(**name) && tools.get(name).is_some())
            .map(|name| (*name).to_string())
            .collect();
        let exposed_set = exposed_tools.iter().cloned().collect();
        Self {
            tools,
            approval,
            exposed_tools: Arc::from(exposed_tools),
            exposed_set,
        }
    }

    fn currently_allowed(&self, call: &ToolCall) -> bool {
        match &self.approval {
            ToolApprovalMode::Auto => true,
            ToolApprovalMode::Manual => false,
            ToolApprovalMode::RequireWhen(predicate) => !predicate(call),
        }
    }

    fn currently_available_tools(&self) -> Vec<String> {
        self.currently_available_tools_for(None)
    }

    fn currently_available_tools_for(&self, requested: Option<(&str, &str)>) -> Vec<String> {
        self.exposed_tools
            .iter()
            .filter(|name| {
                let arguments = requested
                    .filter(|(requested_name, _)| requested_name == name)
                    .map_or_else(|| "{}".to_string(), |(_, arguments)| arguments.to_string());
                self.tools.get(name).is_some()
                    && self.currently_allowed(&ToolCall {
                        id: "ptc-capability-probe".to_string(),
                        name: (*name).clone(),
                        arguments: Some(arguments),
                    })
            })
            .cloned()
            .collect()
    }

    fn unavailable(&self, requested: String, available: Vec<String>) -> HostToolCallError {
        HostToolCallError::Unavailable {
            requested,
            available,
        }
    }
}

impl HostToolInvoker for AgentToolInvoker {
    fn exposed_tools(&self) -> Arc<[String]> {
        self.exposed_tools.clone()
    }

    fn call(
        &self,
        name: String,
        arguments: String,
        context: ToolCallContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<HostToolCallResult, HostToolCallError>> + Send>>
    {
        let available = self.currently_available_tools_for(Some((&name, &arguments)));
        if !self.exposed_set.contains(&name) {
            let error = self.unavailable(name, available);
            return Box::pin(async move { Err(error) });
        }
        if !available.contains(&name) {
            let error = self.unavailable(name, available);
            return Box::pin(async move { Err(error) });
        }
        let Some(tool) = self.tools.get(&name) else {
            let error = self.unavailable(name, available);
            return Box::pin(async move { Err(error) });
        };
        Box::pin(async move {
            if context.cancel.is_cancelled() {
                return Err(HostToolCallError::Aborted(
                    "nested tool call was cancelled".to_string(),
                ));
            }
            let result = tool.execute(arguments, context).await;
            match result {
                Ok(output) => Ok(HostToolCallResult { output }),
                Err(ToolError::InvalidParameters(message)) => {
                    Err(HostToolCallError::InvalidParameters(message))
                }
                Err(ToolError::ExecutionError(message)) => {
                    Err(HostToolCallError::Execution(message))
                }
                Err(ToolError::ResourceLimit(message)) => {
                    Err(HostToolCallError::ResourceLimit(message))
                }
                Err(ToolError::Aborted(message)) => Err(HostToolCallError::Aborted(message)),
            }
        })
    }
}

fn execute_javascript_tool_discovery(
    input: String,
    invoker: Option<AgentToolInvoker>,
) -> std::pin::Pin<Box<dyn Future<Output = ToolResult<String>> + Send>> {
    let started = std::time::Instant::now();
    let span = info_span!(
        "execute_tool",
        tool = coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME,
        input_bytes = input.len(),
        output_bytes = tracing::field::Empty,
        status = tracing::field::Empty,
        error_category = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    );
    Box::pin(
        async move {
            info!("executing tool");
            let result = match serde_json::from_str::<Value>(&input) {
                Ok(Value::Object(object)) if object.is_empty() => match invoker {
                    Some(invoker) => coda_tools::available_tools_result(
                        &invoker.currently_available_tools(),
                    )
                    .map_err(|error| ToolError::ResourceLimit(error.to_string())),
                    None => Err(ToolError::ExecutionError(
                        "PTC_UNAVAILABLE: list_javascript_tools has no persisted capability snapshot"
                            .to_string(),
                    )),
                },
                Ok(Value::Object(_)) => Err(ToolError::InvalidParameters(
                    "expected an empty object".to_string(),
                )),
                Ok(_) => Err(ToolError::InvalidParameters(
                    "expected an empty JSON object".to_string(),
                )),
                Err(error) => Err(ToolError::InvalidParameters(error.to_string())),
            };
            let span = Span::current();
            match &result {
                Ok(output) => {
                    span.record("status", "ok");
                    span.record("output_bytes", output.len());
                }
                Err(ToolError::InvalidParameters(_)) => {
                    span.record("status", "error");
                    span.record("error_category", "invalid_parameters");
                }
                Err(ToolError::ExecutionError(_)) => {
                    span.record("status", "error");
                    span.record("error_category", "execution");
                }
                Err(ToolError::ResourceLimit(_)) => {
                    span.record("status", "error");
                    span.record("error_category", "resource_limit");
                }
                Err(ToolError::Aborted(_)) => {
                    span.record("status", "error");
                    span.record("error_category", "aborted");
                }
            }
            span.record("duration_ms", started.elapsed().as_millis() as u64);
            result
        }
        .instrument(span),
    )
}

/// One tool call's window onto the thread's state.
///
/// `committed` is the thread as the whole batch was dispatched — shared, so
/// sibling calls running concurrently never observe each other. `recorded` is
/// what *this* call has written, which only it can see until the runtime anchors
/// it to the message recording the call.
struct CallState {
    committed: Arc<HashMap<String, serde_json::Value>>,
    recorded: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

impl CallState {
    fn new(committed: Arc<HashMap<String, serde_json::Value>>) -> Self {
        CallState {
            committed,
            recorded: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// What this call wrote, in write order, to be anchored by the caller.
    fn take(&self) -> Vec<(String, serde_json::Value)> {
        std::mem::take(&mut *self.recorded.lock().expect("state mutex poisoned"))
    }
}

impl ThreadState for CallState {
    fn get(&self, kind: &str) -> Option<serde_json::Value> {
        // Its own writes first: within one call, a read-modify-write must see
        // what it just did.
        let recorded = self.recorded.lock().expect("state mutex poisoned");
        recorded
            .iter()
            .rev()
            .find(|(recorded_kind, _)| recorded_kind == kind)
            .map(|(_, value)| value.clone())
            .or_else(|| self.committed.get(kind).cloned())
    }

    fn set(
        &self,
        kind: &str,
        value: serde_json::Value,
    ) -> Result<(), coda_core::tool::HostEffectError> {
        self.recorded
            .lock()
            .expect("state mutex poisoned")
            .push((kind.to_string(), value));
        Ok(())
    }
}

/// What became of an incoming envelope. Only this step can refuse one, so it
/// gets its own outcome rather than widening [`AgentLoopState`] with a case the
/// other steps could never produce.
enum EnvelopeOutcome {
    Next(ResumePoint),
    Done(ResumePoint, Box<TurnEnd>),
    /// Refused: the turn in flight is not finished with this thread, either
    /// because sub-agents still owe it answers or because it is parked on an
    /// approval nobody answered. The turn winds up first; whoever holds the
    /// envelope tries again once it has.
    Deferred(Box<Envelope>, ResumePoint),
}

/// Where a cancelled thread got to.
enum WindUp {
    /// Still owed real replies from sub-agents it already dispatched. It parks
    /// with them recorded, and each reply brings it back to try again.
    Waiting(ResumePoint),
    /// Nothing left to wait for, so this is the end of its work.
    Ended(Box<TurnEnd>),
}

/// What a turn owes the outside world once its state is durable. Handlers
/// describe it; [`AgentLoop::persist_and_announce`] is the only place that
/// carries it out, and only after the checkpoint write succeeds.
#[derive(Default)]
struct TurnEnd {
    /// The event that announces this turn is over. `None` when the run exits
    /// without announcing anything — an unexpected envelope, say.
    event: Option<AgentEvent>,
    /// The result handed back to the caller. Sub-agents only; a root agent has
    /// nobody to answer.
    reply: Option<Envelope>,
}

/// How a single call to the model finished.
enum GenerationOutcome {
    Completed(Box<AssistantMessage>),
    Aborted,
    Failed(String),
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
    /// The turn is not over: this thread dispatched sub-agent calls and is
    /// parked until their answers arrive. The agent goes back to its inbox, but
    /// only replies may reach that thread while its other envelopes wait. Carries the
    /// turn so the wait can be capped once that turn has been asked to stop.
    AwaitingReplies(TurnId),
    /// The envelope was handed back unconsumed. It is held until the turn in
    /// flight has wound up, then delivered again in the order it arrived.
    ///
    /// `awaiting` carries what a wind-up that could not finish would otherwise
    /// have reported as [`Self::AwaitingReplies`]: the turn still owed answers.
    /// Both halves have to travel together, because the envelope only stays put
    /// while the thread is known to be waiting — otherwise the queue hands it
    /// straight back to the refusal that returned it.
    Deferred {
        envelope: Box<Envelope>,
        awaiting: Option<TurnId>,
    },
    /// The exit barrier was already set when the turn started or checked.
    ExitAcquired,
}

struct AgentLoop<'a, C: LLMProvider + Clone> {
    runtime: AgentRuntime,
    agent: &'a mut Agent,
    cancel: CancellationToken,
    config: AgentRunConfig<C>,
    thread_id: ThreadId,
    /// The turn every event this thread emits belongs to. Refreshed whenever an
    /// incoming envelope opens new work; events raised while cleaning up after
    /// the previous one still carry the turn they are cleaning up.
    turn: TurnId,
    reply_target: Option<ReplyTarget>,
    /// How this thread was addressed by whoever spawned it. Unlike
    /// `reply_target` these outlive the call that set them: they are the thread's
    /// place in the tree, not a pending obligation.
    origin_thread: Option<OriginThread>,
}

/// A thread's position under its parent: who spawned it, and the name its own id
/// was derived from.
#[derive(Debug, Clone)]
struct OriginThread {
    parent_thread_id: String,
    derivation_key: String,
}

impl<'a, C: LLMProvider + Clone> AgentLoop<'a, C> {
    /// The turn this thread's history places it in, falling back to the turn
    /// the loop was entered with.
    ///
    /// The fallback covers exactly one case: a thread with no history at all —
    /// newly opened, or a stateless sub-agent's `Agent` between threads — which
    /// is in no turn of its own until its prompt is appended. Everywhere else
    /// the history answers, and the two agree.
    async fn thread_turn(&self) -> TurnId {
        self.agent.current_turn().await.unwrap_or(self.turn)
    }

    async fn run(&mut self, envelope: Option<Envelope>) -> Result<TurnOutcome, String> {
        // Load stored checkpoint and scatter its fields into the appropriate
        // locations. After this block the stored type is gone — only the
        // `resume_point` local variable carries forward.
        let stored = match self
            .runtime
            .session_storage
            .load_checkpoint(self.thread_id.as_ref())
            .await
        {
            Ok(stored) => stored,
            Err(err) => {
                self.runtime
                    .emit_event(
                        self.agent.name.clone(),
                        self.thread_id.clone(),
                        self.turn,
                        AgentEvent::PersistFailed(format!("failed to load checkpoint: {err}")),
                    )
                    .await;
                if self.runtime.is_root_thread(&self.thread_id) {
                    self.runtime.turn_gate.close(self.turn);
                }
                return Err(err);
            }
        };
        let (mut resume_point, mut suspended_at): (ResumePoint, jiff::Timestamp) =
            if let Some(stored) = stored {
                self.agent
                    .restore_history(stored.messages, stored.state)
                    .await;
                self.reply_target = stored.reply_target;
                self.origin_thread = stored.parent_thread_id.zip(stored.derivation_key).map(
                    |(parent_thread_id, derivation_key)| OriginThread {
                        parent_thread_id,
                        derivation_key,
                    },
                );
                (stored.resume_point.into(), stored.suspended_at)
            } else {
                // The Agent instance may be reused across different thread IDs
                // (e.g. stateless subagent calls), so we must clear any stale
                // in-memory state to avoid leaking conversation across threads.
                self.agent.restore_history(vec![], vec![]).await;
                self.reply_target = None;
                self.origin_thread = None;
                (ResumePoint::Generation, jiff::Timestamp::default())
            };
        // A restored thread carries the turn its last message was written
        // under; a fresh one carries none yet and keeps the turn this loop was
        // entered with (the session's active turn) until its prompt lands.
        self.turn = self.thread_turn().await;

        if let Some(envelope) = envelope {
            let is_user_task = matches!(envelope.body, EnvelopeBody::Task { .. });
            match self.handle_envelope(resume_point, envelope).await {
                EnvelopeOutcome::Next(rp) => {
                    resume_point = rp;
                    self.turn = self.thread_turn().await;
                    // Persist the user prompt immediately so a mid-turn snapshot
                    // (reconnect, crash) already contains it; the event stream
                    // never carries user messages. Restricted to root user tasks
                    // to avoid extra writes on sub-agent ToolCall envelopes.
                    if is_user_task
                        && !self
                            .persist_and_announce(
                                resume_point.clone(),
                                suspended_at,
                                TurnEnd::default(),
                            )
                            .await
                    {
                        return Ok(TurnOutcome::Completed);
                    }
                }
                EnvelopeOutcome::Done(rp, owed) => {
                    self.persist_and_announce(rp, suspended_at, *owed).await;
                    return Ok(TurnOutcome::Completed);
                }
                EnvelopeOutcome::Deferred(envelope, rp) => {
                    // Deferring means the turn in flight has just been asked to
                    // stop, so it winds up here rather than waiting to be
                    // entered again — the envelope has to come back to a thread
                    // that is no longer holding the old turn's work, or it would
                    // walk into the same refusal and defer forever. Whether that
                    // wind-up ends the turn or only parks it, the state goes to
                    // storage before the envelope goes back, so a crash in
                    // between leaves the same picture the caller returns to.
                    let turn = self.turn;
                    let (rp, owed, awaiting) = match self.wind_up(rp).await {
                        WindUp::Waiting(rp) => (rp, TurnEnd::default(), Some(turn)),
                        WindUp::Ended(end) => (ResumePoint::Generation, *end, None),
                    };
                    let announced_ending = owed.event.is_some();
                    if self.persist_and_announce(rp, suspended_at, owed).await
                        && announced_ending
                        && self.runtime.is_root_thread(&self.thread_id)
                    {
                        self.runtime.turn_gate.close(self.thread_turn().await);
                    }
                    return Ok(TurnOutcome::Deferred { envelope, awaiting });
                }
            }
        }

        let mut exit_acquired = false;
        let mut suspended = false;
        let mut owed = TurnEnd::default();
        let turn = self.turn;
        loop {
            if self.runtime.exit_barrier.is_exiting() {
                exit_acquired = true;
                break;
            }
            // Checked every time round rather than once on entry: the mark can
            // arrive while this thread is mid-turn, and a thread woken by a
            // reply comes back through here to find it.
            if self.runtime.turn_gate.is_cancelled(turn) {
                match self.wind_up(std::mem::take(&mut resume_point)).await {
                    WindUp::Waiting(rp) => resume_point = rp,
                    WindUp::Ended(end) => {
                        resume_point = ResumePoint::Generation;
                        owed = *end;
                    }
                }
                break;
            }
            let current = std::mem::take(&mut resume_point);
            match current {
                ResumePoint::Generation => {
                    let generation = match self.maybe_auto_compact().await {
                        Ok(()) => self.handle_generation().await,
                        Err(error) => self.generation_failed(error.to_string()),
                    };
                    match generation {
                        AgentLoopState::Next(rp @ ResumePoint::PendingApproval { .. }) => {
                            suspended_at = jiff::Timestamp::now();
                            resume_point = rp;
                        }
                        AgentLoopState::Next(rp) => resume_point = rp,
                        AgentLoopState::Done(rp, end) => {
                            resume_point = rp;
                            owed = *end;
                            break;
                        }
                    }
                }
                ResumePoint::ToolExecution(tool_execution_state) => {
                    match self.handle_tool_execution(tool_execution_state).await {
                        AgentLoopState::Next(rp @ ResumePoint::ToolExecution(_)) => {
                            resume_point = rp;
                            break;
                        }
                        AgentLoopState::Next(rp) => resume_point = rp,
                        AgentLoopState::Done(rp, end) => {
                            resume_point = rp;
                            owed = *end;
                            break;
                        }
                    }
                }
                ResumePoint::PendingApproval {
                    parent_message_id,
                    pending_approval_calls,
                    pending_calls,
                } => {
                    let has_pending = !pending_approval_calls.is_empty();
                    let pending = PendingApproval {
                        thread_id: self.thread_id.as_ref().to_string(),
                        agent_name: self.agent.name.to_string(),
                        parent_message_id,
                        calls: pending_approval_calls
                            .iter()
                            .map(|prepared| prepared.tool_call.clone())
                            .collect(),
                        suspended_at,
                    };
                    resume_point = ResumePoint::PendingApproval {
                        parent_message_id,
                        pending_approval_calls,
                        pending_calls,
                    };
                    if has_pending {
                        suspended = true;
                        owed.event = Some(AgentEvent::Suspended(pending));
                    }
                    break;
                }
            }
        }

        // A turn is over when the root announces an ending for it. Suspension
        // announces one too but the turn is only parked, and a thread that
        // broke out to wait for sub-agent replies announces nothing at all —
        // both leave the turn on the books.
        let announced_ending = owed.event.is_some() && !suspended;
        let awaiting_replies = matches!(
            &resume_point,
            ResumePoint::ToolExecution(state) if !state.pending_replies.is_empty()
        );
        let persisted = self
            .persist_and_announce(resume_point, suspended_at, owed)
            .await;
        if announced_ending && persisted && self.runtime.is_root_thread(&self.thread_id) {
            self.runtime.turn_gate.close(self.thread_turn().await);
        }
        Ok(if exit_acquired {
            TurnOutcome::ExitAcquired
        } else if suspended && persisted {
            TurnOutcome::Suspended
        } else if awaiting_replies && persisted {
            TurnOutcome::AwaitingReplies(turn)
        } else {
            // A failed write never announced the suspension, and left a stale
            // checkpoint behind for anything still to come; going idle beats
            // parking on a picture that is no longer true.
            TurnOutcome::Completed
        })
    }

    /// Make this thread's state durable, then hand out what the turn owes the
    /// outside world. Every checkpoint write in `run` goes through here, so the
    /// ordering holds by construction rather than by each call site remembering
    /// it — and an empty `owed` is an ordinary input, not a special case.
    ///
    /// Returns `false` when the write failed, in which case nothing was
    /// announced and the turn stops where it stands.
    async fn persist_and_announce(
        &self,
        resume_point: ResumePoint,
        suspended_at: jiff::Timestamp,
        owed: TurnEnd,
    ) -> bool {
        if let Err(err) = self.save_checkpoint(resume_point, suspended_at).await {
            error!(
                "Failed to save checkpoint for thread {}: {}",
                self.thread_id.as_ref(),
                err
            );
            self.runtime
                .emit_event(
                    self.agent.name.clone(),
                    self.thread_id.clone(),
                    self.turn,
                    AgentEvent::PersistFailed(err),
                )
                .await;
            return false;
        }
        if let Some(event) = owed.event {
            self.runtime
                .emit_event(
                    self.agent.name.clone(),
                    self.thread_id.clone(),
                    self.turn,
                    event,
                )
                .await;
        }
        // Last, always: the reply is what lets the caller move on, so anything
        // this thread wants seen must already be out before it goes.
        if let Some(reply) = owed.reply
            && let Err(err) = self.runtime.send_message(reply).await
        {
            error!("Failed to send reply: {}", err);
        }
        true
    }

    async fn save_checkpoint(
        &self,
        resume_point: ResumePoint,
        suspended_at: jiff::Timestamp,
    ) -> Result<(), String> {
        let stored = StoredCheckpoint {
            thread_id: self.thread_id.as_ref().to_string(),
            agent_name: self.agent.name.to_string(),
            parent_thread_id: self
                .origin_thread
                .as_ref()
                .map(|origin| origin.parent_thread_id.clone()),
            derivation_key: self
                .origin_thread
                .as_ref()
                .map(|origin| origin.derivation_key.clone()),
            reply_target: self.reply_target.clone(),
            messages: self.agent.history().await,
            state: self.agent.state_entries().await,
            resume_point: resume_point.into(),
            suspended_at,
        };
        self.runtime
            .session_storage
            .save_checkpoint(self.thread_id.as_ref().to_string(), stored)
            .await
    }

    /// Bring a cancelled thread to a stop without inventing anything.
    ///
    /// Calls that never left this thread are written off here — nothing else
    /// will ever produce a result for them. Calls already dispatched to a
    /// sub-agent are not: that sub-agent is winding up too and will answer for
    /// itself. Answering on its behalf is exactly the break this protocol
    /// exists to prevent, since its own work may still be reaching storage.
    async fn wind_up(&mut self, resume_point: ResumePoint) -> WindUp {
        match resume_point {
            ResumePoint::Generation => {}
            ResumePoint::ToolExecution(mut state) => {
                for tc in state.tool_calls.drain(..) {
                    self.write_off(tc.tool_call.id, tc.tool_call.name).await;
                }
                if !state.pending_replies.is_empty() {
                    return WindUp::Waiting(ResumePoint::ToolExecution(state));
                }
            }
            ResumePoint::PendingApproval {
                mut pending_approval_calls,
                mut pending_calls,
                ..
            } => {
                for tc in pending_approval_calls.drain(..) {
                    self.write_off(tc.tool_call.id, tc.tool_call.name).await;
                }
                for tc in pending_calls.drain(..) {
                    self.write_off(tc.tool_call.id, tc.tool_call.name).await;
                }
            }
        }
        let interrupted = self.interrupted_calls().await;
        let target = self.reply_target.take();
        WindUp::Ended(Box::new(self.turn_end(
            target,
            Some(AgentEvent::Aborted(if interrupted.is_empty() {
                AbortedTarget::Generation
            } else {
                AbortedTarget::ToolCalls(interrupted)
            })),
            ToolOutput::Err("Aborted by user".to_string()),
            true,
        )))
    }

    /// The calls this turn ended as aborted, in history order. Read back from
    /// history rather than tallied along the way, because a wind-up finishes in
    /// several passes — one per reply still owed — and the marker has to name
    /// everything the abort caught, not just what the last pass touched.
    async fn interrupted_calls(&self) -> Vec<String> {
        let turn = self.thread_turn().await;
        self.agent
            .history()
            .await
            .into_iter()
            .filter_map(|entry| match entry.message {
                Message::Tool(tool)
                    if entry.turn_id == turn
                        && matches!(tool.outcome, ToolCallOutcome::Aborted) =>
                {
                    Some(tool.id)
                }
                _ => None,
            })
            .collect()
    }

    /// Whether the sub-agent this call went to is still working on it here.
    ///
    /// The thread is derived the way the dispatch derived it rather than looked
    /// up, because a sub-agent that has not reached a write point yet owns no
    /// checkpoint to find. `false` means the work went away with an earlier
    /// process and no answer is ever coming.
    fn is_being_answered(&self, parent_message_id: MessageId, pending: &PendingReply) -> bool {
        let Some(subagent) = self.agent.subagents.get(&pending.tool_name) else {
            return false;
        };
        let derivation_key = if subagent.mode == SubAgentMode::Stateless {
            MessageOrigin {
                message_id: parent_message_id,
                call_id: pending.call_id.clone(),
            }
            .derivation_key()
        } else {
            subagent.name.clone()
        };
        self.runtime
            .calls
            .is_answering(&ThreadId::from_uuid5(&self.thread_id, &derivation_key))
    }

    /// Record a call that will never run, so history holds no tool call without
    /// an answer.
    async fn write_off(&mut self, call_id: String, tool_name: String) {
        self.add_tool_message(ToolMessage::new(
            call_id,
            tool_name,
            ToolOutput::Err("Tool execution was interrupted by the user".to_string()),
            ToolCallOutcome::Aborted,
            None,
        ))
        .await;
    }

    /// Record a settled local tool call. A call that observed cancellation
    /// keeps whatever partial output it salvaged and is marked `Aborted`;
    /// anything else keeps the outcome it started with. Returns whether the
    /// call was recorded as aborted.
    async fn settle_local_tool(
        &mut self,
        tc: PendingToolCall,
        started_at: jiff::Timestamp,
        result: ToolResult<String>,
        call_state: &CallState,
        artifacts: Vec<coda_core::llm::ToolArtifact>,
    ) -> bool {
        let (output, outcome) = match result {
            Ok(output) => (ToolOutput::Ok(output), tc.outcome),
            Err(ToolError::Aborted(reason)) => (ToolOutput::Err(reason), ToolCallOutcome::Aborted),
            Err(err) => (
                ToolOutput::Err(format!("Tool execution error: {}", err)),
                tc.outcome,
            ),
        };
        let aborted = matches!(outcome, ToolCallOutcome::Aborted);
        let succeeded = matches!(output, ToolOutput::Ok(_));
        // A call that did not succeed establishes no new value, whatever it
        // recorded on the way: `set` says "this is what it is now", and a call
        // that failed or was cut short never got to mean that.
        let recorded = if succeeded {
            call_state.take()
        } else {
            Vec::new()
        };
        self.add_message_with_state(
            ToolMessage::new(
                tc.tool_call.id,
                tc.tool_call.name,
                output,
                outcome,
                Some(started_at),
            )
            .with_artifacts(if succeeded { artifacts } else { Vec::new() }),
            recorded,
        )
        .await;
        aborted
    }

    /// Append a tool message that recorded no state: a write-off, a rejection,
    /// a missing tool, or a sub-agent's reply.
    async fn add_tool_message(&mut self, message: ToolMessage) {
        self.add_message_with_state(message, Vec::new()).await
    }

    /// Append a tool message and its recorded state, then emit the matching
    /// `ToolCallEnd`. Keeping them together ensures state never loses its
    /// message anchor, while the event keeps consumers in sync with history.
    async fn add_message_with_state(
        &mut self,
        message: ToolMessage,
        recorded: Vec<(String, Value)>,
    ) {
        self.agent
            .add_message_with_state(Message::Tool(message.clone()), recorded)
            .await;
        self.runtime
            .emit_event(
                self.agent.name.clone(),
                self.thread_id.clone(),
                self.turn,
                AgentEvent::ToolCallEnd(message),
            )
            .await;
    }

    async fn handle_envelope(
        &mut self,
        resume_point: ResumePoint,
        envelope: Envelope,
    ) -> EnvelopeOutcome {
        // Only a `ToolCall` states this thread's place in the tree; other
        // envelopes say nothing about it, so they leave whatever the checkpoint
        // restored intact rather than clearing it.
        if let Some(origin_thread) = origin_thread_from_envelope(&envelope) {
            self.origin_thread = Some(origin_thread);
        }
        match resume_point {
            ResumePoint::Generation => {
                let Some((turn_id, user)) = opening_user_message(&envelope.body) else {
                    warn!("unexpected envelope {:?}", envelope);
                    return EnvelopeOutcome::Done(ResumePoint::Generation, Box::default());
                };
                // `None` for a root task, whose sender is the user rather than
                // a calling agent.
                self.reply_target = reply_target_from_envelope(&envelope);
                self.agent.add_user_message(turn_id, user).await;
                EnvelopeOutcome::Next(ResumePoint::Generation)
            }
            ResumePoint::ToolExecution(mut tool_execution) => {
                if !tool_execution.pending_replies.is_empty() {
                    match &envelope {
                        Envelope {
                            body:
                                EnvelopeBody::Reply {
                                    call_id,
                                    output,
                                    aborted,
                                },
                            ..
                        } => {
                            // The obligation ends here rather than at delivery:
                            // until the answer has actually been taken, the
                            // caller must still treat it as coming.
                            if let Sender::Agent { thread_id, .. } = &envelope.from {
                                self.runtime.calls.end(thread_id);
                            }
                            if let Some(pos) = tool_execution
                                .pending_replies
                                .iter()
                                .position(|call| &call.call_id == call_id)
                            {
                                let tc = tool_execution.pending_replies.remove(pos);
                                // How the call was authorised, unless the
                                // answerer says it never got to finish.
                                let outcome = if *aborted {
                                    ToolCallOutcome::Aborted
                                } else {
                                    tc.outcome
                                };
                                self.add_tool_message(ToolMessage::new(
                                    tc.call_id,
                                    tc.tool_name,
                                    output.clone(),
                                    outcome,
                                    Some(tc.started_at),
                                ))
                                .await;
                            }
                        }
                        Envelope {
                            body: EnvelopeBody::Task { .. } | EnvelopeBody::ToolCall { .. },
                            ..
                        } => {
                            // New work arrived while calls are still outstanding.
                            // Anything whose sub-agent went away with a previous
                            // process is written off here — nothing in this one
                            // will ever answer it. The rest are still being
                            // worked on and have to answer for themselves.
                            let parent_message_id = tool_execution.parent_message_id;
                            let mut still_answering = Vec::new();
                            for pending in tool_execution.pending_replies.drain(..) {
                                if self.is_being_answered(parent_message_id, &pending) {
                                    still_answering.push(pending);
                                    continue;
                                }
                                self.add_tool_message(ToolMessage::new(
                                    pending.call_id,
                                    pending.tool_name,
                                    ToolOutput::Err(
                                        "Tool execution was interrupted by the user".to_string(),
                                    ),
                                    ToolCallOutcome::Aborted,
                                    Some(pending.started_at),
                                ))
                                .await;
                            }
                            if !still_answering.is_empty() {
                                // A second call reached the same thread while
                                // its first call still has live children. Stop
                                // the turn and let those children answer before
                                // retrying the held call. A root Task cannot
                                // reach this branch: runtime admission rejects
                                // it while the turn is active.
                                debug_assert!(matches!(
                                    &envelope.body,
                                    EnvelopeBody::ToolCall { .. }
                                ));
                                tool_execution.pending_replies = still_answering;
                                self.runtime.cancel_turn(self.thread_turn().await).await;
                                return EnvelopeOutcome::Deferred(
                                    Box::new(envelope),
                                    ResumePoint::ToolExecution(tool_execution),
                                );
                            }
                            self.reply_target = reply_target_from_envelope(&envelope);
                            if let Some((turn_id, user)) = opening_user_message(&envelope.body) {
                                self.agent.add_user_message(turn_id, user).await;
                            }
                            return EnvelopeOutcome::Next(ResumePoint::Generation);
                        }
                        _ => {
                            warn!("expect a reply envelope but got a {:?}", envelope);
                            return EnvelopeOutcome::Done(
                                ResumePoint::ToolExecution(tool_execution),
                                Box::default(),
                            );
                        }
                    }
                }
                if tool_execution.pending_replies.is_empty() {
                    return EnvelopeOutcome::Next(ResumePoint::Generation);
                }
                EnvelopeOutcome::Next(ResumePoint::ToolExecution(tool_execution))
            }
            ResumePoint::PendingApproval {
                parent_message_id,
                mut pending_approval_calls,
                mut pending_calls,
            } => {
                match &envelope.body {
                    EnvelopeBody::ToolCall { .. } => {
                        // Another call arrived for a thread parked on an
                        // approval. Discarding the parked calls
                        // here and carrying straight on would end the turn
                        // without ever ending it: nothing announces that it
                        // stopped, so wind it up before retrying the envelope.
                        //
                        // Unlike the sub-agent case there is nobody to tell: a
                        // thread parked on an approval has dispatched nothing,
                        // so the only work to stop is its own, and it is already
                        // running. Marking the turn would only leave a stale
                        // abort in this agent's own control queue for the
                        // replayed envelope to walk into. A root Task cannot
                        // arrive here because the suspended turn still owns
                        // the session's single active-turn slot.
                        warn!(
                            "Received envelope while suspended for approval; stopping the turn holding {} pending call(s)",
                            pending_approval_calls.len()
                        );
                        EnvelopeOutcome::Deferred(
                            Box::new(envelope),
                            ResumePoint::PendingApproval {
                                parent_message_id,
                                pending_approval_calls,
                                pending_calls,
                            },
                        )
                    }
                    EnvelopeBody::Resume(decision) => {
                        // A decision answers one batch, and a call it does not
                        // name counts as rejected below — so one meant for an
                        // *earlier* batch would reject this one outright. That
                        // is not hypothetical: a second submit of the same
                        // approval (a double click, a retry after a reconnect)
                        // arrives once this thread has run those calls and
                        // suspended on the model's next batch.
                        //
                        // The batch is identified by the assistant message that
                        // asked for it, never by which call ids the decision
                        // mentions: ids are only unique within one assistant
                        // message, so consecutive batches reuse them and a
                        // stale decision would look like a live one. Stay
                        // suspended and re-announce, so the caller
                        // resynchronizes on what is really parked here.
                        if decision.parent_message_id != parent_message_id {
                            warn!(
                                "ignoring a resume for batch {}; this thread is parked on {}",
                                decision.parent_message_id, parent_message_id
                            );
                            return EnvelopeOutcome::Next(ResumePoint::PendingApproval {
                                parent_message_id,
                                pending_approval_calls,
                                pending_calls,
                            });
                        }
                        let resolution_map: HashMap<String, ToolCallResolution> =
                            decision.resolutions.iter().cloned().collect();
                        for tc in pending_approval_calls.drain(..) {
                            let PreparedToolCall {
                                tool_call,
                                metadata,
                            } = tc;
                            let resolution = resolution_map
                                .get(&tool_call.id)
                                .cloned()
                                .unwrap_or(ToolCallResolution::Rejected { reason: None });
                            match resolution {
                                ToolCallResolution::Execute => {
                                    pending_calls.push_back(PendingToolCall {
                                        tool_call,
                                        outcome: ToolCallOutcome::Approved,
                                        metadata,
                                    });
                                }
                                ToolCallResolution::Resolved(output) => {
                                    self.add_tool_message(ToolMessage::new(
                                        tool_call.id,
                                        tool_call.name,
                                        output,
                                        ToolCallOutcome::Resolved,
                                        None,
                                    ))
                                    .await;
                                }
                                ToolCallResolution::Rejected { reason } => {
                                    self.add_tool_message(ToolMessage::new(
                                        tool_call.id,
                                        tool_call.name,
                                        ToolOutput::Err(
                                            reason
                                                .clone()
                                                .unwrap_or_else(|| "Rejected by user".to_string()),
                                        ),
                                        ToolCallOutcome::Rejected { reason },
                                        None,
                                    ))
                                    .await;
                                }
                            }
                        }
                        EnvelopeOutcome::Next(ResumePoint::ToolExecution(ToolExecutionState {
                            parent_message_id,
                            pending_replies: vec![],
                            tool_calls: pending_calls.clone(),
                        }))
                    }
                    _ => {
                        warn!(
                            "unexpected envelope while suspended for approval: {:?}",
                            envelope
                        );
                        EnvelopeOutcome::Done(
                            ResumePoint::PendingApproval {
                                parent_message_id,
                                pending_approval_calls,
                                pending_calls,
                            },
                            Box::default(),
                        )
                    }
                }
            }
        }
    }

    /// Checked once per entry into [`ResumePoint::Generation`], on every
    /// thread. Once the last recorded usage reaches the profile's threshold,
    /// asks [`compaction::cutoff`] for a complete turn boundary or, when the
    /// current turn itself has grown too large, a complete tool-batch boundary.
    /// A fresh task-opening user message remains protected until the agent has
    /// made progress. The result is appended silently before the next LLM call.
    ///
    /// On failure, appends only a failure record — no boundary moves, so a
    /// later over-threshold check in the same turn retries.
    async fn maybe_auto_compact(&mut self) -> Result<(), message_view::InvalidHistory> {
        // Cheap check first: avoids `Agent::history`'s full clone when usage
        // is nowhere near threshold, the overwhelmingly common case.
        let Some(usage) = self.agent.last_usage().await else {
            return Ok(());
        };
        if usage.total_tokens < self.config.profile.auto_compact_threshold_tokens {
            return Ok(());
        }
        let history = self.agent.history().await;
        let current_turn = self.thread_turn().await;
        let protect_from = message_view::model_view(&history)
            .last()
            .filter(|entry| {
                entry.turn_id == current_turn && matches!(entry.message, Message::User(_))
            })
            .map(|entry| entry.message.message_id());
        let Some(cutoff) = compaction::cutoff(
            &history,
            Some(current_turn),
            protect_from,
            Some(self.config.profile.auto_compact_threshold_tokens),
        )?
        else {
            return Ok(());
        };
        self.runtime
            .emit_event(
                self.agent.name.clone(),
                self.thread_id.clone(),
                self.turn,
                AgentEvent::CompactionStart,
            )
            .await;
        let request = compaction::summary_request(
            self.config.profile.model.clone(),
            self.config.profile.max_completion_tokens,
            self.config.profile.reasoning_effort.clone(),
            message_view::model_view(&history).take(cutoff.model_view_len),
            "",
        );

        let outcome = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Ok(()),
            outcome = tokio::time::timeout(AUTO_COMPACT_SUMMARY_TIMEOUT, self.summarize(request)) => {
                outcome.unwrap_or_else(|_| {
                    Err(format!(
                        "the provider did not answer within {AUTO_COMPACT_SUMMARY_TIMEOUT:?}"
                    ))
                })
            }
        };
        let message = match outcome {
            Ok(summary) => compaction::summary_message(
                cutoff.coverage_message_id,
                compaction::Trigger::Auto,
                &summary,
            ),
            Err(reason) => {
                warn!(thread_id = %self.thread_id.as_ref(), "auto-compaction failed: {reason}");
                compaction::failure_message(&reason)
            }
        };
        let Message::Compaction(record) = message.clone() else {
            unreachable!("compaction writes only Message::Compaction")
        };
        self.agent.add_message(message).await;
        // Lets the hub fold this into the live snapshot at turn settle.
        self.runtime
            .emit_event(
                self.agent.name.clone(),
                self.thread_id.clone(),
                self.turn,
                AgentEvent::CompactionEnd(record),
            )
            .await;
        Ok(())
    }

    /// One provider round-trip turning `request` into a summary; no
    /// intermediate chunks are announced.
    async fn summarize(&self, request: ChatCompletionRequest) -> Result<String, String> {
        let mut stream = std::pin::pin!(self.config.profile.provider.stream(request));
        while let Some(event) = stream.next().await {
            match event.map_err(|err| err.to_string())? {
                LLMStreamEvent::Completed(message) => {
                    return if message.content.trim().is_empty() {
                        Err("the provider returned an empty summary".to_string())
                    } else {
                        Ok(message.content.clone())
                    };
                }
                LLMStreamEvent::ContentChunk(_) | LLMStreamEvent::ReasoningChunk(_) => {}
            }
        }
        Err("the provider closed the stream without a summary".to_string())
    }

    fn requires_approval(&self, call: &ToolCall) -> bool {
        match &self.config.tool_approval {
            ToolApprovalMode::Auto => false,
            ToolApprovalMode::Manual => true,
            ToolApprovalMode::RequireWhen(predicate) => predicate(call),
        }
    }

    /// Build the provider-visible tools and the hidden capability snapshot from
    /// the same policy decision. The snapshot is later attached to a returned
    /// programmatic call and survives approvals/checkpoints.
    fn generation_tools(&self) -> (Vec<ToolDefinition>, Option<Vec<String>>) {
        let descriptors = self.agent.tools.descriptors();
        let runner_configured = descriptors
            .iter()
            .any(|tool| tool.name == coda_tools::RUN_JAVASCRIPT_TOOL_NAME);
        let by_name: HashMap<_, _> = descriptors
            .iter()
            .cloned()
            .map(|tool| (tool.name.clone(), tool))
            .collect();
        let eligible: Vec<ToolDefinition> = coda_tools::PROGRAMMATIC_TOOL_NAMES
            .iter()
            .filter_map(|name| {
                let descriptor = by_name.get(*name)?;
                let probe = ToolCall {
                    id: "ptc-capability-probe".to_string(),
                    name: (*name).to_string(),
                    arguments: Some("{}".to_string()),
                };
                (!self.requires_approval(&probe)).then(|| descriptor.clone())
            })
            .collect();
        let candidate_snapshot = (runner_configured && !eligible.is_empty()).then(|| {
            eligible
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>()
        });
        let snapshot = candidate_snapshot.and_then(|names| {
            let longest = names
                .iter()
                .max_by_key(|name| name.len())
                .expect("nonempty capability snapshot");
            match (
                coda_tools::available_tools_result(&names),
                coda_tools::tool_unavailable_message(longest, &names),
            ) {
                (Ok(_), Ok(_)) => Some(names),
                (discovery, unavailable) => {
                    let error = discovery
                        .err()
                        .or_else(|| unavailable.err())
                        .expect("one capability message validation failed");
                    error!(%error, "omitting programmatic tools because capability metadata exceeds its limit");
                    None
                }
            }
        });

        let mut request_tools = Vec::with_capacity(descriptors.len() + 1);
        for descriptor in descriptors {
            if descriptor.name == coda_tools::RUN_JAVASCRIPT_TOOL_NAME {
                if snapshot.is_some() {
                    request_tools.push(coda_tools::list_javascript_tools_definition());
                    request_tools.push(coda_tools::run_javascript_definition());
                }
            } else {
                request_tools.push(descriptor);
            }
        }
        request_tools.extend(self.agent.subagents.descriptors());
        (request_tools, snapshot)
    }

    async fn handle_generation(&mut self) -> AgentLoopState {
        let thread_id = self.thread_id.clone();
        let messages = match self.agent.messages().await {
            Ok(messages) => messages,
            Err(error) => return self.generation_failed(error.to_string()),
        };
        let (request_tools, ptc_snapshot) = self.generation_tools();
        let request = ChatCompletionRequest {
            model: self.config.profile.model.clone(),
            max_completion_tokens: self.config.profile.max_completion_tokens,
            temperature: self.config.profile.temperature,
            reasoning_effort: self.config.profile.reasoning_effort.clone(),
            messages,
            tools: request_tools,
        };
        let started_at = jiff::Timestamp::now();
        self.runtime
            .emit_event(
                self.agent.name.clone(),
                thread_id.clone(),
                self.turn,
                AgentEvent::LLMStart(request.clone()),
            )
            .await;

        let provider = self.config.profile.provider.clone();
        let mut llm_stream = std::pin::pin!(provider.stream(request));
        let mut partial_content = String::new();
        let mut partial_reasoning = String::new();
        let mut reasoning_ended_at: Option<jiff::Timestamp> = None;
        let outcome = loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    // The aborted partial message goes to history AND out as an
                    // LLMEnd before the Aborted marker, so event consumers see
                    // every history write; `Aborted` alone settles the turn.
                    if !partial_content.is_empty() || !partial_reasoning.is_empty() {
                        let ended_at = jiff::Timestamp::now();
                        let has_reasoning = !partial_reasoning.is_empty();
                        let content = if partial_content.is_empty() {
                            "[Generation was interrupted by the user]".to_string()
                        } else {
                            partial_content + "\n[Generation was interrupted by the user]"
                        };
                        let message = coda_core::llm::AssistantMessage {
                            message_id: MessageId::new(),
                            content,
                            tool_calls: Vec::new(),
                            usage: None,
                            reasoning_content: has_reasoning.then_some(partial_reasoning),
                            reasoning_continuation: None,
                            reasoning_ended_at: has_reasoning.then_some(reasoning_ended_at.unwrap_or(ended_at)),
                            aborted: true,
                            started_at,
                            ended_at,
                        };
                        self.agent.add_message(Message::Assistant(message.clone())).await;
                        self.runtime.emit_event(self.agent.name.clone(), thread_id.clone(), self.turn, AgentEvent::LLMEnd(message)).await;
                    }
                    break GenerationOutcome::Aborted;
                }
                event = llm_stream.next() => {
                    match event {
                        Some(Ok(LLMStreamEvent::ContentChunk(chunk))) => {
                            if !partial_reasoning.is_empty() && reasoning_ended_at.is_none() {
                                reasoning_ended_at = Some(jiff::Timestamp::now());
                            }
                            partial_content.push_str(&chunk);
                            self.runtime.emit_event(self.agent.name.clone(), thread_id.clone(), self.turn,AgentEvent::LLMContentChunk(chunk)).await;
                        }
                        Some(Ok(LLMStreamEvent::ReasoningChunk(chunk))) => {
                            partial_reasoning.push_str(&chunk);
                            self.runtime.emit_event(self.agent.name.clone(), thread_id.clone(), self.turn,AgentEvent::LLMReasoningChunk(chunk)).await;
                        }
                        Some(Ok(LLMStreamEvent::Completed(message))) => break GenerationOutcome::Completed(message),
                        Some(Err(err)) => break GenerationOutcome::Failed(err.to_string()),
                        None => break GenerationOutcome::Failed(StreamError::InvalidResponse(
                            "LLM stream ended without Completed event".to_string(),
                        )
                        .to_string()),
                    }
                }
            }
        };
        let mut assistant_message = match outcome {
            GenerationOutcome::Completed(message) => *message,
            GenerationOutcome::Aborted => {
                let target = self.reply_target.take();
                return AgentLoopState::Done(
                    ResumePoint::Generation,
                    Box::new(self.turn_end(
                        target,
                        Some(AgentEvent::Aborted(AbortedTarget::Generation)),
                        ToolOutput::Err("Aborted by user".to_string()),
                        true,
                    )),
                );
            }
            GenerationOutcome::Failed(err) => return self.generation_failed(err),
        };

        let ended_at = jiff::Timestamp::now();
        if assistant_message.reasoning_content.is_some() {
            assistant_message.reasoning_ended_at = Some(reasoning_ended_at.unwrap_or(ended_at));
        }
        assistant_message.started_at = started_at;
        assistant_message.ended_at = ended_at;
        self.agent
            .add_message(Message::Assistant(assistant_message.clone()))
            .await;

        if assistant_message.tool_calls.is_empty() {
            let content = assistant_message.content.clone();
            let target = self.reply_target.take();
            return AgentLoopState::Done(
                ResumePoint::Generation,
                Box::new(self.turn_end(
                    target,
                    Some(AgentEvent::LLMEnd(assistant_message)),
                    ToolOutput::Ok(content),
                    false,
                )),
            );
        }

        // Mid-turn: the loop carries on from here, so this one goes out now.
        self.runtime
            .emit_event(
                self.agent.name.clone(),
                thread_id,
                self.turn,
                AgentEvent::LLMEnd(assistant_message.clone()),
            )
            .await;

        // Every call in this batch came from this one message; the batch state
        // carries its id so a sub-agent dispatched later — possibly after an
        // approval suspension or a restart — can still record what triggered it.
        let parent_message_id = assistant_message.message_id;
        let prepared_calls: Vec<PreparedToolCall> = assistant_message
            .tool_calls
            .into_iter()
            .map(|tool_call| PreparedToolCall {
                metadata: if matches!(
                    tool_call.name.as_str(),
                    coda_tools::RUN_JAVASCRIPT_TOOL_NAME
                        | coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME
                ) {
                    ptc_snapshot.clone().map(|exposed_tools| {
                        ToolExecutionMetadata::ProgrammaticToolCalling { exposed_tools }
                    })
                } else {
                    None
                },
                tool_call,
            })
            .collect();
        let (pending_approval_calls, auto_calls) = match &self.config.tool_approval {
            ToolApprovalMode::Auto => (vec![], prepared_calls),
            ToolApprovalMode::Manual => (prepared_calls, vec![]),
            ToolApprovalMode::RequireWhen(predicate) => prepared_calls
                .into_iter()
                .partition(|call| predicate(&call.tool_call)),
        };
        let auto_calls: VecDeque<_> = auto_calls
            .into_iter()
            .map(|call| PendingToolCall {
                tool_call: call.tool_call,
                outcome: ToolCallOutcome::Auto,
                metadata: call.metadata,
            })
            .collect();
        if pending_approval_calls.is_empty() {
            AgentLoopState::Next(ResumePoint::ToolExecution(ToolExecutionState {
                parent_message_id,
                pending_replies: vec![],
                tool_calls: auto_calls,
            }))
        } else {
            AgentLoopState::Next(ResumePoint::PendingApproval {
                parent_message_id,
                pending_approval_calls: pending_approval_calls.into(),
                pending_calls: auto_calls,
            })
        }
    }

    /// Package a turn ending: what to announce, plus `output` handed back to
    /// whoever called this thread as a tool. A root agent has no caller — its
    /// `target` is `None` and `output` goes nowhere.
    fn turn_end(
        &self,
        target: Option<ReplyTarget>,
        event: Option<AgentEvent>,
        output: ToolOutput,
        aborted: bool,
    ) -> TurnEnd {
        TurnEnd {
            event,
            reply: target.map(|target| {
                Envelope::with_id(|id| Envelope {
                    id,
                    from: Sender::Agent {
                        name: self.agent.name.clone(),
                        thread_id: self.thread_id.clone(),
                    },
                    to: Receiver {
                        name: target.sender_name,
                        thread_id: ThreadId::from(target.sender_thread_id),
                    },
                    reply_to: Some(target.envelope_id),
                    body: EnvelopeBody::Reply {
                        call_id: target.call_id,
                        output,
                        aborted,
                    },
                })
            }),
        }
    }

    fn generation_failed(&mut self, error: String) -> AgentLoopState {
        error!(thread_id = %self.thread_id.as_ref(), "generation failed: {error}");
        let target = self.reply_target.take();
        // A sub-agent's failure travels as its reply; only a root agent, with
        // nobody to answer, announces it as an event.
        let event = target.is_none().then(|| AgentEvent::Error(error.clone()));
        AgentLoopState::Done(
            ResumePoint::Generation,
            Box::new(self.turn_end(target, event, ToolOutput::Err(error), false)),
        )
    }

    async fn handle_tool_execution(
        &mut self,
        mut tool_execution: ToolExecutionState,
    ) -> AgentLoopState {
        let concurrent_stateful =
            concurrent_stateful_subagents(self.agent, &tool_execution.tool_calls);
        // Tracks local tool calls that have not yet completed, keyed by tool call id.
        // The timestamp records when execution began, for the result's duration.
        // Handed to every sub-agent dispatched below so their messages group with
        // the submission that ultimately caused them.
        let turn_id = self.thread_turn().await;
        // One view of the thread for the whole batch, taken before any of it
        // runs. A tool that derives its state from the conversation reads this
        // rather than keeping a store, so it must not be able to observe its
        // siblings landing — the batch runs concurrently and has no order to
        // observe.
        let committed = Arc::new(self.agent.state_snapshot().await);
        let mut pending_local: HashMap<String, (PendingToolCall, jiff::Timestamp)> = HashMap::new();
        let mut futures = futures::stream::FuturesUnordered::new();
        for tc in &tool_execution.tool_calls {
            if let Some(subagent) = self.agent.subagents.get(&tc.tool_call.name) {
                if subagent.mode == SubAgentMode::Stateful
                    && concurrent_stateful.contains(&subagent.name)
                {
                    // reject concurrent calls to stateful subagent
                    self.add_tool_message(ToolMessage::new(
                        tc.tool_call.id.clone(),
                        tc.tool_call.name.clone(),
                        ToolOutput::Err(format!("Concurrent invocation of sub-agent '{}' is not allowed. Call it sequentially.", tc.tool_call.name)),
                        tc.outcome.clone(),
                        None,
                    )).await;
                    continue;
                }

                let origin = MessageOrigin {
                    message_id: tool_execution.parent_message_id,
                    call_id: tc.tool_call.id.clone(),
                };
                let derivation_key = if subagent.mode == SubAgentMode::Stateless {
                    // Stateless: each invocation gets its own thread, so derive
                    // from what identifies the invocation. The call id alone
                    // won't do — it is only unique within one assistant message,
                    // so reusing it across turns would derive the same thread
                    // twice and the second invocation would inherit the first
                    // one's conversation (nothing ever deletes a thread's
                    // checkpoint).
                    origin.derivation_key()
                } else {
                    // Stateful: derive from the agent name so the sub-agent's
                    // session persists across calls in the same conversation.
                    subagent.name.clone()
                };
                let subagent_thread_id = ThreadId::from_uuid5(&self.thread_id, &derivation_key);
                let subagent_tool_call_envelope = Envelope::with_id(|id| Envelope {
                    id,
                    from: Sender::Agent {
                        name: self.agent.name.clone(),
                        thread_id: self.thread_id.clone(),
                    },
                    to: Receiver {
                        // The tool name is prefixed (`agent__foo`); route by the
                        // bare agent name the runtime registered.
                        name: subagent.name.clone(),
                        thread_id: subagent_thread_id,
                    },
                    reply_to: None,
                    body: EnvelopeBody::ToolCall {
                        call_id: origin.call_id.clone(),
                        parent_message_id: origin.message_id,
                        derivation_key: derivation_key.clone(),
                        turn_id,
                        // Sub-agent tools always take {"task": "..."} — extract the string.
                        task: serde_json::from_str::<serde_json::Value>(
                            tc.tool_call.arguments.as_deref().unwrap_or("{}"),
                        )
                        .ok()
                        .and_then(|v| v["task"].as_str().map(String::from))
                        .unwrap_or_default(),
                    },
                });
                let call_envelope_id = subagent_tool_call_envelope.id.clone();
                let ret = self.runtime.send_message(subagent_tool_call_envelope).await;
                if let Err(err) = ret {
                    error!(
                        "Failed to send tool call to subagent {}, error: {}",
                        tc.tool_call.name, err
                    );
                    self.add_tool_message(ToolMessage::new(
                        tc.tool_call.id.clone(),
                        tc.tool_call.name.clone(),
                        ToolOutput::Err(format!(
                            "Failed to dispatch to subagent '{}': {}",
                            tc.tool_call.name, err
                        )),
                        tc.outcome.clone(),
                        None,
                    ))
                    .await;
                } else {
                    self.runtime
                        .emit_event(
                            self.agent.name.clone(),
                            self.thread_id.clone(),
                            self.turn,
                            AgentEvent::ToolCallStart(tc.tool_call.clone()),
                        )
                        .await;
                    tool_execution.pending_replies.push(PendingReply {
                        call_id: tc.tool_call.id.clone(),
                        call_envelope_id,
                        tool_name: tc.tool_call.name.clone(),
                        outcome: tc.outcome.clone(),
                        started_at: jiff::Timestamp::now(),
                    });
                }
            } else if tc.tool_call.name == coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME
                || self.agent.tools.get(&tc.tool_call.name).is_some()
            {
                let started_at = jiff::Timestamp::now();
                self.runtime
                    .emit_event(
                        self.agent.name.clone(),
                        self.thread_id.clone(),
                        self.turn,
                        AgentEvent::ToolCallStart(tc.tool_call.clone()),
                    )
                    .await;
                pending_local.insert(tc.tool_call.id.clone(), (tc.clone(), started_at));
                let tc = tc.clone();
                // A child of the turn's token: aborting the turn cancels every
                // in-flight tool, and (later) a single tool can be cancelled
                // without touching its siblings.
                let call_state = Arc::new(CallState::new(committed.clone()));
                let mut ctx = ToolCallContext::new(self.cancel.child_token(), call_state.clone());
                let invoker = match &tc.metadata {
                    Some(ToolExecutionMetadata::ProgrammaticToolCalling { exposed_tools }) => {
                        Some(AgentToolInvoker::new(
                            self.agent.tools.clone(),
                            self.config.tool_approval.clone(),
                            exposed_tools.clone(),
                        ))
                    }
                    None => None,
                };
                let execution: std::pin::Pin<Box<dyn Future<Output = ToolResult<String>> + Send>> =
                    if tc.tool_call.name == coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME {
                        execute_javascript_tool_discovery(
                            tc.tool_call.arguments.clone().unwrap_or_default(),
                            invoker,
                        )
                    } else {
                        if tc.tool_call.name == coda_tools::RUN_JAVASCRIPT_TOOL_NAME
                            && let Some(invoker) = invoker
                        {
                            ctx = ctx.with_invoker(Arc::new(invoker));
                        }
                        self.agent
                            .tools
                            .get(&tc.tool_call.name)
                            .expect("ordinary local tool was checked above")
                            .execute(
                                tc.tool_call.arguments.clone().unwrap_or_default(),
                                ctx.clone(),
                            )
                    };
                let future = async move {
                    let output = execution.await;
                    (tc, started_at, output, call_state, ctx.take_artifacts())
                };
                futures.push(future);
            } else {
                // No such tool
                self.add_tool_message(ToolMessage::new(
                    tc.tool_call.id.clone(),
                    tc.tool_call.name.clone(),
                    ToolOutput::Err(format!("No such tool: {}", tc.tool_call.name)),
                    tc.outcome.clone(),
                    None,
                ))
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
                Some((tc, started_at, result, call_state, artifacts)) = futures.next() => {
                    pending_local.remove(&tc.tool_call.id);
                    self.settle_local_tool(tc, started_at, result, &call_state, artifacts).await;
                }
            }
        };
        tool_execution.tool_calls.clear();

        if aborted {
            // Aborted ToolMessages go to history AND out as ToolCallEnd events
            // before the Aborted marker, so event consumers see every history
            // write; `Aborted` alone settles the turn.
            let mut aborted_ids: Vec<String> = Vec::new();
            // The tools saw the cancellation through their context token; give
            // them a grace period to tear down their work (kill child
            // processes, collect partial output) and settle, rather than
            // dropping their futures mid-flight. Tools that complete for real
            // during the drain keep their genuine results.
            let grace = tokio::time::sleep(TOOL_ABORT_GRACE);
            tokio::pin!(grace);
            while !futures.is_empty() {
                tokio::select! {
                    biased;
                    _ = &mut grace => break,
                    Some((tc, started_at, result, call_state, artifacts)) = futures.next() => {
                        pending_local.remove(&tc.tool_call.id);
                        let id = tc.tool_call.id.clone();
                        if self.settle_local_tool(tc, started_at, result, &call_state, artifacts).await {
                            aborted_ids.push(id);
                        }
                    }
                }
            }
            // Whatever outlived the grace period gets dropped mid-flight and
            // recorded with a generic interruption message.
            drop(futures);
            aborted_ids.extend(pending_local.keys().cloned());
            for (id, (tc, started_at)) in pending_local {
                self.add_tool_message(ToolMessage::new(
                    id,
                    tc.tool_call.name,
                    ToolOutput::Err("Tool execution was interrupted by the user".to_string()),
                    ToolCallOutcome::Aborted,
                    Some(started_at),
                ))
                .await;
            }
            if !tool_execution.pending_replies.is_empty() {
                // The sub-agents already dispatched are winding up on their own
                // and will answer for themselves. Park with those calls still
                // outstanding; the loop comes back here on each reply, and the
                // ending goes out once the last one lands.
                return AgentLoopState::Next(ResumePoint::ToolExecution(tool_execution));
            }
            let target = self.reply_target.take();
            return AgentLoopState::Done(
                ResumePoint::Generation,
                Box::new(self.turn_end(
                    target,
                    Some(AgentEvent::Aborted(AbortedTarget::ToolCalls(aborted_ids))),
                    ToolOutput::Err("Aborted by user".to_string()),
                    true,
                )),
            );
        }

        if !tool_execution.pending_replies.is_empty() {
            AgentLoopState::Next(ResumePoint::ToolExecution(tool_execution.clone()))
        } else {
            AgentLoopState::Next(ResumePoint::Generation)
        }
    }
}

/// The user message that opens the work an envelope asks for, or `None` for
/// envelopes that continue existing work (`Reply`, `Resume`) rather than start
/// any.
///
/// A root task carries the id minted at the request boundary, and that id also
/// names the turn it begins. A sub-agent invocation mints its own message id
/// here — this is that message's only construction point, so there is no second
/// copy to stay in sync with — inherits the caller's turn, and records the
/// calling thread's tool call as its origin, which is what later lets a rewind
/// tell this invocation's messages from another invocation's in the same thread.
fn opening_user_message(body: &EnvelopeBody) -> Option<(TurnId, UserMessage)> {
    match body {
        EnvelopeBody::Task {
            message_id,
            task,
            images,
        } => Some((
            TurnId::from(*message_id),
            UserMessage::with_images(*message_id, task.clone(), images),
        )),
        EnvelopeBody::ToolCall {
            call_id,
            parent_message_id,
            turn_id,
            task,
            ..
        } => Some((
            *turn_id,
            UserMessage::from_subagent_call(
                MessageId::new(),
                task.clone(),
                MessageOrigin {
                    message_id: *parent_message_id,
                    call_id: call_id.clone(),
                },
            ),
        )),
        EnvelopeBody::Reply { .. } | EnvelopeBody::Resume(_) => None,
    }
}

/// This thread's place under its caller, as announced by a `ToolCall` envelope.
/// `None` for anything else, since only being called as a tool gives a thread a
/// parent.
fn origin_thread_from_envelope(envelope: &Envelope) -> Option<OriginThread> {
    match (&envelope.from, &envelope.body) {
        (Sender::Agent { thread_id, .. }, EnvelopeBody::ToolCall { derivation_key, .. }) => {
            Some(OriginThread {
                parent_thread_id: thread_id.as_ref().to_string(),
                derivation_key: derivation_key.clone(),
            })
        }
        _ => None,
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
        // Key by the resolved (bare) agent name so prefixed and bare tool-name
        // forms that point at the same stateful sub-agent are counted together.
        if let Some(subagent) = agent.subagents.get(&tc.tool_call.name)
            && subagent.mode == crate::SubAgentMode::Stateful
        {
            *counts.entry(subagent.name.clone()).or_insert(0usize) += 1;
        }
    }
    counts
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name))
        .collect()
}

#[cfg(test)]
#[path = "driver_tests/mod.rs"]
mod tests;
