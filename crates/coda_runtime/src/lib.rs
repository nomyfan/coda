use coda_agent::{
    AbortedTarget, Agent, AgentEvent, AgentID, Envelope, RunConfig, Sender, SubAgentMode,
    SubAgentObject,
};
use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, Message, StreamError,
    ToolCall, ToolCallOutcome, ToolMessage, ToolOutput, UserMessage,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, instrument};
use uuid::Uuid;

#[derive(Clone)]
pub enum AgentControl {
    Abort,
    /// Shut down the agent loop entirely.
    Exit,
}

#[derive(Debug)]
pub enum SendCommandError {
    AgentNotFound,
    ChannelClosed,
}

impl std::fmt::Display for SendCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendCommandError::AgentNotFound => write!(f, "Agent not found"),
            SendCommandError::ChannelClosed => write!(f, "Channel closed"),
        }
    }
}

impl std::error::Error for SendCommandError {}

pub struct AgentHandle {
    agent_id: AgentID,
    control_sender: mpsc::Sender<AgentControl>,
    message_sender: mpsc::Sender<Envelope>,
    event_sender: broadcast::Sender<AgentEvent>,
}

impl AgentHandle {
    pub fn agent_id(&self) -> &AgentID {
        &self.agent_id
    }

    pub async fn send_command(&self, cmd: AgentControl) -> Result<(), SendCommandError> {
        self.control_sender
            .send(cmd)
            .await
            .map_err(|_| SendCommandError::ChannelClosed)
    }

    /// Send a message to this agent, triggering a new turn.
    pub async fn send_message(&self, envelope: Envelope) -> Result<(), SendCommandError> {
        self.message_sender
            .send(envelope)
            .await
            .map_err(|_| SendCommandError::ChannelClosed)
    }

    /// Opt-in subscribe to this agent's event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_sender.subscribe()
    }
}

struct AgentEntry {
    control_sender: mpsc::Sender<AgentControl>,
    message_sender: mpsc::Sender<Envelope>,
}

#[derive(Clone)]
pub struct AgentRuntime {
    agents: Arc<Mutex<HashMap<AgentID, AgentEntry>>>,
    /// Global event bus — all agents forward their events here.
    global_event_tx: broadcast::Sender<(AgentID, AgentEvent)>,
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRuntime {
    pub fn new() -> Self {
        let (global_event_tx, _) = broadcast::channel(256);
        AgentRuntime {
            agents: Arc::new(Mutex::new(HashMap::new())),
            global_event_tx,
        }
    }

    /// Subscribe to events from all agents (including those spawned later).
    pub fn subscribe(&self) -> broadcast::Receiver<(AgentID, AgentEvent)> {
        self.global_event_tx.subscribe()
    }

    pub async fn spawn_agent<P: LLMProvider + Clone>(
        &self,
        agent: Agent,
        config: RunConfig<P>,
    ) -> AgentHandle {
        let agent_id = AgentID::new();
        let (control_tx, control_rx) = mpsc::channel(10);
        let (message_tx, message_rx) = mpsc::channel(32);
        let (event_tx, _) = broadcast::channel(64);

        self.agents.lock().await.insert(
            agent_id.clone(),
            AgentEntry {
                control_sender: control_tx.clone(),
                message_sender: message_tx.clone(),
            },
        );

        let event_tx_for_task = event_tx.clone();
        let agent_id_for_task = agent_id.clone();
        let runtime = self.clone();
        tokio::spawn(async move {
            // Forward per-agent events to the global event bus.
            let forward_task = {
                let mut event_rx = event_tx_for_task.subscribe();
                let global_tx = runtime.global_event_tx.clone();
                let aid = agent_id_for_task.clone();
                tokio::spawn(async move {
                    while let Ok(event) = event_rx.recv().await {
                        // Ignore send errors — there may temporarily be no subscribers
                        // between turns.
                        let _ = global_tx.send((aid.clone(), event));
                    }
                })
            };

            run_agent(
                agent_id_for_task,
                agent,
                control_rx,
                message_rx,
                event_tx_for_task,
                &config,
                &runtime,
            )
            .await;

            forward_task.abort();
        });

        AgentHandle {
            agent_id,
            control_sender: control_tx,
            message_sender: message_tx,
            event_sender: event_tx,
        }
    }

    /// Send a control command to a specific agent by ID.
    pub async fn send_command(
        &self,
        agent_id: &AgentID,
        cmd: AgentControl,
    ) -> Result<(), SendCommandError> {
        let agents = self.agents.lock().await;
        let entry = agents
            .get(agent_id)
            .ok_or(SendCommandError::AgentNotFound)?;
        entry
            .control_sender
            .send(cmd)
            .await
            .map_err(|_| SendCommandError::ChannelClosed)
    }

    /// Send a message to a specific agent by ID, triggering a new turn.
    pub async fn send_message(
        &self,
        agent_id: &AgentID,
        envelope: Envelope,
    ) -> Result<(), SendCommandError> {
        let agents = self.agents.lock().await;
        let entry = agents
            .get(agent_id)
            .ok_or(SendCommandError::AgentNotFound)?;
        entry
            .message_sender
            .send(envelope)
            .await
            .map_err(|_| SendCommandError::ChannelClosed)
    }

    async fn remove_agent(&self, agent_id: &AgentID) {
        self.agents.lock().await.remove(agent_id);
    }

    /// Spawn an agent with a specific ID if it doesn't already exist (idempotent).
    pub async fn get_or_spawn_agent_with_id<P: LLMProvider + Clone>(
        &self,
        agent_id: AgentID,
        agent: Agent,
        config: RunConfig<P>,
    ) {
        let mut agents = self.agents.lock().await;
        if agents.contains_key(&agent_id) {
            return;
        }

        let (control_tx, control_rx) = mpsc::channel(10);
        let (message_tx, message_rx) = mpsc::channel(32);
        let (event_tx, _) = broadcast::channel(64);

        agents.insert(
            agent_id.clone(),
            AgentEntry {
                control_sender: control_tx,
                message_sender: message_tx,
            },
        );
        drop(agents);

        let event_tx_for_task = event_tx.clone();
        let agent_id_for_task = agent_id.clone();
        let runtime = self.clone();
        tokio::spawn(async move {
            let forward_task = {
                let mut event_rx = event_tx_for_task.subscribe();
                let global_tx = runtime.global_event_tx.clone();
                let aid = agent_id_for_task.clone();
                tokio::spawn(async move {
                    while let Ok(event) = event_rx.recv().await {
                        let _ = global_tx.send((aid.clone(), event));
                    }
                })
            };

            run_agent(
                agent_id_for_task,
                agent,
                control_rx,
                message_rx,
                event_tx_for_task,
                &config,
                &runtime,
            )
            .await;

            forward_task.abort();
        });
    }

    /// Broadcast a control command to all agents.
    pub async fn broadcast_command(&self, cmd: AgentControl) {
        let agents = self.agents.lock().await;
        for entry in agents.values() {
            let _ = entry.control_sender.send(cmd.clone()).await;
        }
    }
}

#[instrument(skip_all, fields(agent_id = ?agent_id, agent_name = agent.name(), model = config.model, thread_id = config.thread_id))]
async fn run_agent<P: LLMProvider + Clone>(
    agent_id: AgentID,
    mut agent: Agent,
    mut control_rx: mpsc::Receiver<AgentControl>,
    mut message_rx: mpsc::Receiver<Envelope>,
    event_tx: broadcast::Sender<AgentEvent>,
    config: &RunConfig<P>,
    runtime: &AgentRuntime,
) {
    // Wait for incoming messages, then process them in the agent loop.
    while let Some(envelope) = message_rx.recv().await {
        // Add the envelope body as a user message to the conversation.
        agent
            .add_message(Message::User(UserMessage(envelope.body)))
            .await;

        'agent_loop: loop {
            let request = ChatCompletionRequest {
                model: config.model.clone(),
                max_completion_tokens: config.max_completion_tokens,
                temperature: config.temperature,
                messages: agent.messages().await,
                tools: {
                    let mut tools = agent.tools.descriptors();
                    tools.extend(agent.subagents.descriptors());
                    tools
                },
            };

            let _ = event_tx.send(AgentEvent::LLMStart(request.clone()));

            let mut assistant_message = None;
            let mut partial_content = String::new();
            let mut aborted_in_llm = false;
            let mut llm_error: Option<StreamError> = None;

            // LLM stream
            {
                let mut llm_stream = std::pin::pin!(config.provider.stream(request));
                'llm_stream: loop {
                    // We select on both the LLM stream and the command receiver, so that we can react to commands (like abort) while waiting for the LLM response.
                    tokio::select! {
                        biased;
                        Some(cmd) = control_rx.recv() => {
                            match cmd {
                                AgentControl::Abort | AgentControl::Exit => {
                                    aborted_in_llm = true;
                                    break 'llm_stream;
                                }
                            }
                        }
                        event = llm_stream.next() => {
                            match event {
                                Some(Ok(LLMStreamEvent::ContentChunk(s))) => {
                                    partial_content.push_str(&s);
                                    let _ = event_tx.send(AgentEvent::LLMContentChunk(s));
                                }
                                Some(Ok(LLMStreamEvent::Completed(msg))) => {
                                    assistant_message = Some(msg);
                                    break 'llm_stream;
                                }
                                Some(Err(e)) => {
                                    llm_error = Some(e);
                                    break 'llm_stream;
                                }
                                None => {
                                    break 'llm_stream;
                                }
                            }
                        }
                    }
                }
            }

            if let Some(err) = llm_error {
                let err_string = format!("{}", err);
                error!("LLM stream error in agent {}", err_string);
                let _ = event_tx.send(AgentEvent::Error(err_string));
                break 'agent_loop;
            }

            if aborted_in_llm {
                // Only write a partial AssistantMessage if there was actual content.
                if !partial_content.is_empty() {
                    let partial_msg = AssistantMessage {
                        content: partial_content,
                        aborted: true,
                        ..Default::default()
                    };
                    agent.add_message(Message::Assistant(partial_msg)).await;
                    let _ = event_tx.send(AgentEvent::Aborted(AbortedTarget::Generation));
                    break 'agent_loop;
                }
            }

            match assistant_message.ok_or_else(|| {
                StreamError::InvalidResponse("LLM stream ended without Completed event".into())
            }) {
                Err(err) => {
                    let _ = event_tx.send(AgentEvent::Error(format!("{}", err)));
                    break 'agent_loop;
                }
                Ok(assistant_message) => {
                    agent
                        .add_message(Message::Assistant(assistant_message.clone()))
                        .await;
                    let _ = event_tx.send(AgentEvent::LLMEnd(assistant_message.clone()));

                    let stop = assistant_message.tool_calls.is_empty();

                    if !assistant_message.tool_calls.is_empty() {
                        // TODO: HITL for tool calls
                        if let Err(true) = execute_tool_calls(
                            &agent_id,
                            &mut agent,
                            assistant_message.tool_calls,
                            &mut control_rx,
                            &event_tx,
                            config,
                            runtime,
                        )
                        .await
                        {
                            let _ = event_tx
                                .send(AgentEvent::Aborted(AbortedTarget::ToolCalls(vec![])));
                            break 'agent_loop;
                        }
                    }

                    if stop {
                        if let Sender::Agent(to) = &envelope.from {
                            let reply = Envelope {
                                id: Uuid::new_v4().to_string(),
                                from: Sender::Agent(agent_id.clone()),
                                to: Sender::Agent(to.clone()),
                                reply_to: Some(envelope.id.clone()),
                                body: assistant_message.content,
                            };
                            let _ = event_tx.send(AgentEvent::AgentToAgent(reply));
                        }
                        break 'agent_loop;
                    }
                }
            }
        } // 'agent_loop
    } // while let

    info!("Agent exiting");
    // Unregister this agent from the runtime.
    runtime.remove_agent(&agent_id).await;
}

#[instrument(skip_all, fields(caller_agent_id = ?caller_agent_id))]
async fn execute_tool_calls<P: LLMProvider + Clone>(
    caller_agent_id: &AgentID,
    agent: &mut Agent,
    tool_calls: Vec<ToolCall>,
    control_receiver: &mut mpsc::Receiver<AgentControl>,
    event_sender: &broadcast::Sender<AgentEvent>,
    config: &RunConfig<P>,
    runtime: &AgentRuntime,
) -> Result<(), bool> {
    // Detect duplicate subagent calls within the same batch.
    let mut subagent_call_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for tc in &tool_calls {
        if let Some(sa) = agent.subagents.get(&tc.name)
            && sa.mode() == SubAgentMode::Stateful
        {
            *subagent_call_counts.entry(tc.name.clone()).or_insert(0) += 1;
        }
    }
    let concurrent_subagents: std::collections::HashSet<String> = subagent_call_counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name)
        .collect();

    let tool_futs = futures::stream::FuturesUnordered::new();
    let mut pending_ids: HashMap<String, String> = HashMap::new();
    for tc in tool_calls {
        pending_ids.insert(tc.id.clone(), tc.name.clone());
        let _ = event_sender.send(AgentEvent::ToolCallStart(tc.clone()));

        let tool = agent.tools.get(&tc.name);
        let subagent = agent.subagents.get(&tc.name);
        let is_concurrent_call = concurrent_subagents.contains(&tc.name);
        tool_futs.push(async move {
            if is_concurrent_call {
                return ToolMessage {
                    id: tc.id,
                    name: tc.name.clone(),
                    output: ToolOutput::Err(format!(
                        "Concurrent invocation of sub-agent '{}' is not allowed. \
                        You called this sub-agent more than once in the same tool-call batch. \
                        Call sub-agents sequentially — one at a time.",
                        tc.name
                    )),
                    outcome: ToolCallOutcome::Auto,
                };
            }
            let output = match tool {
                Some(t) => match t.execute(tc.arguments.unwrap_or_default()).await {
                    Ok(s) => ToolOutput::Ok(s),
                    Err(e) => ToolOutput::Err(e.to_string()),
                },
                None => match subagent {
                    Some(subagent) => match tc.arguments.as_deref() {
                        Some(s) => {
                            let task = serde_json::from_str::<serde_json::Value>(s)
                                .ok()
                                .and_then(|v| v.get("task")?.as_str().map(String::from))
                                .unwrap_or_else(|| s.to_string());
                            match run_subagent(caller_agent_id, &subagent, &task, config, runtime)
                                .await
                            {
                                Ok(content) => ToolOutput::Ok(content),
                                Err(e) => ToolOutput::Err(e),
                            }
                        }
                        None => ToolOutput::Err("Missing arguments".to_string()),
                    },
                    None => ToolOutput::Err(format!("Tool '{}' not found", tc.name)),
                },
            };
            ToolMessage {
                id: tc.id,
                name: tc.name,
                output,
                outcome: ToolCallOutcome::Auto, // TODO: no HITL now
            }
        });
    }

    let mut aborted = false;
    let mut tool_futs = std::pin::pin!(tool_futs);

    loop {
        tokio::select! {
            biased;
            Some(cmd) = control_receiver.recv() => {
                match cmd {
                    AgentControl::Abort | AgentControl::Exit => {
                        aborted = true;
                        break;
                    }
                }
            }
            result = tool_futs.next() => {
                match result {
                    Some(tool_message) => {
                        pending_ids.remove(&tool_message.id);
                        agent.add_message(Message::Tool(tool_message.clone())).await;
                        let _ = event_sender.send(AgentEvent::ToolCallEnd(tool_message));
                    }
                    None => break, // All tool calls have completed
                }
            }
        }
    }

    if aborted {
        // Write aborted results for unfinished tool calls.
        let aborted_ids: Vec<String> = pending_ids.keys().cloned().collect();
        let aborted_messages: Vec<Message> = pending_ids
            .into_iter()
            .map(|(id, name)| {
                Message::Tool(ToolMessage {
                    id,
                    name,
                    output: ToolOutput::Err("Aborted by user".to_string()),
                    outcome: ToolCallOutcome::Aborted,
                })
            })
            .collect();
        agent.add_messages(aborted_messages).await;
        let _ = event_sender.send(AgentEvent::Aborted(AbortedTarget::ToolCalls(aborted_ids)));
    }

    Ok(())
}

// TODO: 因为通过 channel 进行通信，会导致 trace 丢失上下级关系
#[instrument(skip_all, fields(caller_agent_id = ?caller_agent_id, subagent_name = subagent.name(), task = task))]
fn run_subagent<'a, P: LLMProvider + Clone>(
    caller_agent_id: &'a AgentID,
    subagent: &'a Arc<dyn SubAgentObject>,
    task: &'a str,
    config: &'a RunConfig<P>,
    runtime: &'a AgentRuntime,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let mode = subagent.mode();
        // TODO: stateful 的还要 clone 吗？
        let cloned_agent = {
            let agent = subagent.agent().lock().await;
            agent.clone_as_template()
        };

        let caller_agent_id = caller_agent_id.clone();
        let config = config.clone();
        let runtime = runtime.clone();
        let task = task.to_string();
        let subagent_name = subagent.name().to_string();

        // Spawn in a separate task so all captured values are 'static.
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            match mode {
                SubAgentMode::Stateless => {
                    let handle = runtime.spawn_agent(cloned_agent, config).await;
                    let mut event_rx = handle.subscribe();

                    let envelope = Envelope::new(|id| Envelope {
                        id,
                        from: Sender::Agent(caller_agent_id),
                        to: Sender::Agent(handle.agent_id().clone()),
                        reply_to: None,
                        body: task,
                    });
                    if let Err(e) = handle.send_message(envelope).await {
                        let _ = result_tx.send(Err(e.to_string()));
                        return;
                    }

                    let mut last_content = String::new();
                    while let Ok(event) = event_rx.recv().await {
                        match event {
                            AgentEvent::AgentToAgent(reply) => {
                                last_content = reply.body;
                                break;
                            }
                            AgentEvent::Error(e) => {
                                let _ = handle.send_command(AgentControl::Exit).await;
                                let _ = result_tx.send(Err(e));
                                return;
                            }
                            AgentEvent::Aborted(_) => {
                                let _ = handle.send_command(AgentControl::Exit).await;
                                let _ = result_tx.send(Err("Subagent aborted".into()));
                                return;
                            }
                            _ => {}
                        }
                    }
                    // Shut down the subagent loop.
                    let _ = handle.send_command(AgentControl::Exit).await;
                    let _ = result_tx.send(Ok(last_content));
                }
                SubAgentMode::Stateful => {
                    let derived_id = AgentID::from_uuid5(&caller_agent_id, &subagent_name);

                    // Ensure the stateful subagent is alive (idempotent).
                    runtime
                        .get_or_spawn_agent_with_id(derived_id.clone(), cloned_agent, config)
                        .await;

                    // Subscribe to global bus BEFORE sending to avoid missing the reply.
                    let mut global_rx = runtime.subscribe();

                    let envelope = Envelope::new(|id| Envelope {
                        id,
                        from: Sender::Agent(caller_agent_id),
                        to: Sender::Agent(derived_id.clone()),
                        reply_to: None,
                        body: task,
                    });
                    let sent_id = envelope.id.clone();

                    if let Err(e) = runtime.send_message(&derived_id, envelope).await {
                        let _ = result_tx.send(Err(e.to_string()));
                        return;
                    }

                    loop {
                        match global_rx.recv().await {
                            Ok((id, AgentEvent::AgentToAgent(reply)))
                                if id == derived_id
                                    && reply.reply_to.as_deref() == Some(&sent_id) =>
                            {
                                let _ = result_tx.send(Ok(reply.body));
                                return;
                            }
                            Ok((id, AgentEvent::Error(e))) if id == derived_id => {
                                let _ = result_tx.send(Err(e));
                                return;
                            }
                            Ok((id, AgentEvent::Aborted(_))) if id == derived_id => {
                                let _ = result_tx.send(Err("Subagent aborted".into()));
                                return;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                let _ = result_tx.send(Err("Subagent is unavailable".into()));
                                return;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                // Some messages were dropped due to a slow receiver;
                                // the reply may still arrive, so keep waiting.
                            }
                            _ => {}
                        }
                    }
                    // Stateful agent is NOT sent Exit — it persists for future calls.
                }
            }
        });

        result_rx.await.map_err(|e| e.to_string())?
    })
}
