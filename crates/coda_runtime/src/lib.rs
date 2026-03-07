use coda_agent::{AbortedTarget, Agent, AgentEvent, AgentID, Envelope, Sender};
use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMProvider, LLMStreamEvent, Message, StreamError,
    ToolCall, ToolCallOutcome, ToolMessage, ToolOutput, UserMessage,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

#[derive(Clone)]
pub enum AgentCommand {
    Abort,
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

/// Drop this guard to automatically cancel all event-forwarding tasks.
pub struct SubscriptionGuard {
    abort_handles: Vec<tokio::task::AbortHandle>,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        for handle in &self.abort_handles {
            handle.abort();
        }
    }
}

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
    agent_id: AgentID,
    command_sender: mpsc::Sender<AgentCommand>,
    event_sender: broadcast::Sender<AgentEvent>,
}

pub struct AgentRuntime {
    agents: Arc<Mutex<HashMap<AgentID, AgentEntry>>>,
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRuntime {
    pub fn new() -> Self {
        AgentRuntime {
            agents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn spawn_agent<S: Send + 'static>(
        &self,
        agent: Agent<S>,
        llm_provider: impl LLMProvider,
    ) -> AgentHandle {
        let agent_id = AgentID::new();
        let (command_tx, command_rx) = mpsc::channel(10);
        let (event_tx, _) = broadcast::channel(64);

        self.agents.lock().await.insert(
            agent_id.clone(),
            AgentEntry {
                agent_id: agent_id.clone(),
                command_sender: command_tx.clone(),
                event_sender: event_tx.clone(),
            },
        );

        let event_tx_for_task = event_tx.clone();
        let agent_id_for_task = agent_id.clone();
        tokio::spawn(async move {
            run_agent(
                agent_id_for_task,
                agent,
                command_rx,
                event_tx_for_task,
                llm_provider,
            )
            .await;
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

    /// Broadcast a command to all agents.
    pub async fn broadcast_command(&self, cmd: AgentCommand) {
        let agents = self.agents.lock().await;
        for entry in agents.values() {
            let _ = entry.command_sender.send(cmd.clone()).await;
        }
    }

    /// Opt-in subscribe to events from all agents.
    /// Returns a receiver of `(AgentID, AgentEvent)` pairs and a guard.
    /// Drop the guard to stop all forwarding tasks.
    pub async fn subscribe(&self) -> (mpsc::Receiver<(AgentID, AgentEvent)>, SubscriptionGuard) {
        let (tx, rx) = mpsc::channel(64);
        let mut abort_handles = Vec::new();
        let agents = self.agents.lock().await;
        for entry in agents.values() {
            let mut event_rx = entry.event_sender.subscribe();
            let tx = tx.clone();
            let aid = entry.agent_id.clone();
            let handle = tokio::spawn(async move {
                while let Ok(event) = event_rx.recv().await {
                    if tx.send((aid.clone(), event)).await.is_err() {
                        break;
                    }
                }
            });
            abort_handles.push(handle.abort_handle());
        }
        (rx, SubscriptionGuard { abort_handles })
    }
}

async fn run_agent<S>(
    agent_id: AgentID,
    mut agent: Agent<S>,
    mut command_receiver: mpsc::Receiver<AgentCommand>,
    event_sender: broadcast::Sender<AgentEvent>,
    llm_provider: impl LLMProvider,
) {
    // Wait for incoming messages, then process them in the agent loop.
    while let Some(cmd) = command_receiver.recv().await {
        let envelope = match cmd {
            AgentCommand::Message(envelope) => envelope,
            AgentCommand::Abort => continue,
        };

        // Add the envelope body as a user message to the conversation.
        agent
            .add_message(Message::User(UserMessage(envelope.body)))
            .await;

        'agent_loop: loop {
            let request = ChatCompletionRequest {
                // TODO: make this configurable per-agent
                model: "google/gemini-3-flash-preview".to_string(),
                max_completion_tokens: Some(5000),
                temperature: Some(0.7),
                messages: agent.messages().await,
                tools: agent.tools.descriptors(),
            };

            let _ = event_sender.send(AgentEvent::LLMStart(request.clone()));

            let mut assistant_message = None;
            let mut partial_content = String::new();
            let mut aborted_in_llm = false;
            let mut llm_error: Option<StreamError> = None;

            // LLM stream
            {
                let mut llm_stream = std::pin::pin!(llm_provider.stream(request));
                'llm_stream: loop {
                    // We select on both the LLM stream and the command receiver, so that we can react to commands (like abort) while waiting for the LLM response.
                    tokio::select! {
                        biased;
                        Some(cmd) = command_receiver.recv() => {
                            match cmd {
                                AgentCommand::Abort => {
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
                                    let _ = event_sender.send(AgentEvent::LLMContentChunk(s));
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
                let _ = event_sender.send(AgentEvent::Error(format!("{}", err)));
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
                    let _ = event_sender.send(AgentEvent::Aborted(AbortedTarget::Generation));
                    break 'agent_loop;
                }
            }

            match assistant_message.ok_or_else(|| {
                StreamError::InvalidResponse("LLM stream ended without Completed event".into())
            }) {
                Err(err) => {
                    let _ = event_sender.send(AgentEvent::Error(format!("{}", err)));
                    break 'agent_loop;
                }
                Ok(assistant_message) => {
                    agent
                        .add_message(Message::Assistant(assistant_message.clone()))
                        .await;
                    let _ = event_sender.send(AgentEvent::LLMEnd(assistant_message.clone()));

                    let stop = assistant_message.tool_calls.is_empty();

                    if !assistant_message.tool_calls.is_empty() {
                        // TODO: HITL for tool calls
                        if let Err(true) = execute_tool_calls(
                            &mut agent,
                            assistant_message.tool_calls,
                            &mut command_receiver,
                            &event_sender,
                        )
                        .await
                        {
                            let _ = event_sender
                                .send(AgentEvent::Aborted(AbortedTarget::ToolCalls(vec![])));
                            break 'agent_loop;
                        }
                    }

                    if stop {
                        break 'agent_loop;
                    }

                    if let Sender::Agent(to) = &envelope.from {
                        let reply = Envelope {
                            id: Uuid::new_v4().to_string(),
                            from: Sender::Agent(agent_id.clone()),
                            to: Sender::Agent(to.clone()),
                            reply_to: Some(envelope.id.clone()),
                            body: assistant_message.content,
                        };
                        let _ = event_sender.send(AgentEvent::AgentToAgent(reply));
                    }
                }
            }
        } // 'agent_loop
    } // while let
}

async fn execute_tool_calls<S>(
    agent: &mut Agent<S>,
    tool_calls: Vec<ToolCall>,
    command_receiver: &mut mpsc::Receiver<AgentCommand>,
    event_sender: &broadcast::Sender<AgentEvent>,
) -> Result<(), bool> {
    let tool_futs = futures::stream::FuturesUnordered::new();
    let mut pending_ids: HashMap<String, String> = HashMap::new();
    for tc in tool_calls {
        pending_ids.insert(tc.id.clone(), tc.name.clone());
        let _ = event_sender.send(AgentEvent::ToolCallStart(tc.clone()));
        let tool = agent.tools.get(&tc.name);
        tool_futs.push(async {
            let output = match tool {
                Some(t) => match t.execute(tc.arguments.unwrap_or_default()).await {
                    Ok(s) => ToolOutput::Ok(s),
                    Err(e) => ToolOutput::Err(e.to_string()),
                },
                None => ToolOutput::Err(format!("Tool '{}' not found", tc.name)),
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
                    AgentCommand::Abort => {
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
