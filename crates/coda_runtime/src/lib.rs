use coda_agent::{
    AbortedTarget, Agent, AgentEvent, AgentID, Envelope, RunConfig, Sender, SubAgentObject,
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
use tracing::info;
use uuid::Uuid;

#[derive(Clone)]
pub enum AgentCommand {
    Abort,
    /// Shut down the agent loop entirely.
    Exit,
    /// Deliver a message to the agent, triggering a new turn.
    Message(Envelope),
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
    command_sender: mpsc::Sender<AgentCommand>,
    event_sender: broadcast::Sender<AgentEvent>,
}

impl AgentHandle {
    pub fn agent_id(&self) -> &AgentID {
        &self.agent_id
    }

    pub async fn send_command(&self, cmd: AgentCommand) -> Result<(), SendCommandError> {
        self.command_sender
            .send(cmd)
            .await
            .map_err(|_| SendCommandError::ChannelClosed)
    }

    /// Send a message to this agent, triggering a new turn.
    pub async fn send_message(&self, envelope: Envelope) -> Result<(), SendCommandError> {
        self.send_command(AgentCommand::Message(envelope)).await
    }

    /// Opt-in subscribe to this agent's event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_sender.subscribe()
    }
}

struct AgentEntry {
    command_sender: mpsc::Sender<AgentCommand>,
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
        let (command_tx, command_rx) = mpsc::channel(10);
        let (event_tx, _) = broadcast::channel(64);

        self.agents.lock().await.insert(
            agent_id.clone(),
            AgentEntry {
                command_sender: command_tx.clone(),
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
                command_rx,
                event_tx_for_task,
                &config,
                &runtime,
            )
            .await;

            forward_task.abort();
        });

        AgentHandle {
            agent_id,
            command_sender: command_tx,
            event_sender: event_tx,
        }
    }

    /// Send a command to a specific agent by ID.
    pub async fn send_command(
        &self,
        agent_id: &AgentID,
        cmd: AgentCommand,
    ) -> Result<(), SendCommandError> {
        let agents = self.agents.lock().await;
        let entry = agents
            .get(agent_id)
            .ok_or(SendCommandError::AgentNotFound)?;
        entry
            .command_sender
            .send(cmd)
            .await
            .map_err(|_| SendCommandError::ChannelClosed)
    }

    async fn remove_agent(&self, agent_id: &AgentID) {
        self.agents.lock().await.remove(agent_id);
    }

    /// Broadcast a command to all agents.
    pub async fn broadcast_command(&self, cmd: AgentCommand) {
        let agents = self.agents.lock().await;
        for entry in agents.values() {
            let _ = entry.command_sender.send(cmd.clone()).await;
        }
    }
}

async fn run_agent<P: LLMProvider + Clone>(
    agent_id: AgentID,
    mut agent: Agent,
    mut command_rx: mpsc::Receiver<AgentCommand>,
    event_tx: broadcast::Sender<AgentEvent>,
    config: &RunConfig<P>,
    runtime: &AgentRuntime,
) {
    // Wait for incoming messages, then process them in the agent loop.
    while let Some(cmd) = command_rx.recv().await {
        let envelope = match cmd {
            AgentCommand::Message(envelope) => envelope,
            AgentCommand::Abort => continue,
            AgentCommand::Exit => break,
        };

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
                        Some(cmd) = command_rx.recv() => {
                            match cmd {
                                AgentCommand::Abort | AgentCommand::Exit => {
                                    aborted_in_llm = true;
                                    break 'llm_stream;
                                }
                                AgentCommand::Message(_) => {
                                    // Ignore messages while LLM is streaming; they'll be handled in the next turn.
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
                let _ = event_tx.send(AgentEvent::Error(format!("{}", err)));
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
                            &mut command_rx,
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

    info!("Agent {} {:?} exiting", agent.name(), agent_id);
    // Unregister this agent from the runtime.
    runtime.remove_agent(&agent_id).await;
}

async fn execute_tool_calls<P: LLMProvider + Clone>(
    caller_agent_id: &AgentID,
    agent: &mut Agent,
    tool_calls: Vec<ToolCall>,
    command_receiver: &mut mpsc::Receiver<AgentCommand>,
    event_sender: &broadcast::Sender<AgentEvent>,
    config: &RunConfig<P>,
    runtime: &AgentRuntime,
) -> Result<(), bool> {
    let tool_futs = futures::stream::FuturesUnordered::new();
    let mut pending_ids: HashMap<String, String> = HashMap::new();
    for tc in tool_calls {
        pending_ids.insert(tc.id.clone(), tc.name.clone());
        let _ = event_sender.send(AgentEvent::ToolCallStart(tc.clone()));
        let tool = agent.tools.get(&tc.name);
        let subagent = agent.subagents.get(&tc.name);
        tool_futs.push(async {
            let output = match tool {
                Some(t) => match t.execute(tc.arguments.unwrap_or_default()).await {
                    Ok(s) => ToolOutput::Ok(s),
                    Err(e) => ToolOutput::Err(e.to_string()),
                },
                None => match subagent {
                    Some(subagent) => {
                        let task = tc
                            .arguments
                            .as_deref()
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                            .and_then(|v| v.get("task")?.as_str().map(String::from))
                            .unwrap_or_default();
                        match run_subagent(caller_agent_id, &subagent, &task, config, runtime).await
                        {
                            Ok(content) => ToolOutput::Ok(content),
                            Err(e) => ToolOutput::Err(e),
                        }
                    }
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
            Some(cmd) = command_receiver.recv() => {
                match cmd {
                    AgentCommand::Abort | AgentCommand::Exit => {
                        aborted = true;
                        break;
                    }
                    AgentCommand::Message(_) => {
                        // TODO: maybe buffer messages
                        // Ignore messages while executing tool calls.
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

fn run_subagent<'a, P: LLMProvider + Clone>(
    caller_agent_id: &'a AgentID,
    subagent: &'a Arc<dyn SubAgentObject>,
    task: &'a str,
    config: &'a RunConfig<P>,
    runtime: &'a AgentRuntime,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let cloned_agent = {
            let agent = subagent.agent().lock().await;
            agent.clone_as_template()
        };

        let caller_agent_id = caller_agent_id.clone();
        let config = config.clone();
        let runtime = runtime.clone();
        let task = task.to_string();

        // Spawn in a separate task so all captured values are 'static.
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
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
                        let _ = handle.send_command(AgentCommand::Exit).await;
                        let _ = result_tx.send(Err(e));
                        return;
                    }
                    AgentEvent::Aborted(_) => {
                        let _ = handle.send_command(AgentCommand::Exit).await;
                        let _ = result_tx.send(Err("Subagent aborted".into()));
                        return;
                    }
                    _ => {}
                }
            }
            // Shut down the subagent loop.
            let _ = handle.send_command(AgentCommand::Exit).await;
            let _ = result_tx.send(Ok(last_content));
        });

        result_rx.await.map_err(|e| e.to_string())?
    })
}
