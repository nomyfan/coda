use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, CompletionUsage, Message, MessageId, RequestMessage,
    SystemMessage, ToolCall, ToolCallOutcome, ToolDefinition, ToolMessage, ToolOutput, TurnId,
    UserMessage,
};
use coda_core::tool::Tools;

use crate::message_view;
use crate::persist::StateEntry;
use tracing::{debug, error};

/// Prefix applied to sub-agent names when they are exposed to the LLM as tools,
/// mirroring how MCP tools are prefixed with `mcp__`. It makes a sub-agent
/// invocation self-identifying wherever its tool name appears — live events and
/// persisted history alike — so the UI can distinguish it from a built-in tool
/// without any side channel. The runtime strips it back to the bare agent name
/// for routing.
pub const SUBAGENT_TOOL_PREFIX: &str = "agent__";

#[derive(Clone, Default)]
pub enum ToolApprovalMode {
    #[default]
    Auto,
    Manual,
    RequireWhen(Arc<dyn Fn(&ToolCall) -> bool + Send + Sync>),
}

/// Caller's resolution for a single suspended tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolCallResolution {
    /// The agent should execute this call.
    Execute,
    /// The caller already handled it; use this result directly.
    Resolved(ToolOutput),
    /// The caller rejected execution.
    Rejected { reason: Option<String> },
}

/// Caller's response to all suspended tool calls, replacing `ApprovalDecision`.
///
/// A call this does not name counts as rejected, so a decision applies to
/// exactly one batch — hence `parent_message_id`, which says which.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeDecision {
    /// The batch being answered, echoed from
    /// [`PendingApproval::parent_message_id`]. A decision naming a batch the
    /// thread has already run is stale and is ignored rather than applied to
    /// whatever is parked now.
    pub parent_message_id: MessageId,
    pub resolutions: Vec<(String, ToolCallResolution)>,
}

/// Lightweight view of an agent thread waiting for approval.
///
/// This is the public-facing type returned via [`AgentEvent::Suspended`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub thread_id: String,
    pub agent_name: String,
    /// The assistant message that asked for these calls, which is what
    /// identifies the batch. A `call_id` cannot: it is only unique within one
    /// assistant message, so consecutive batches routinely reuse one.
    pub parent_message_id: MessageId,
    pub calls: Vec<ToolCall>,
    pub suspended_at: jiff::Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyTarget {
    pub envelope_id: String,
    pub sender_name: String,
    pub sender_thread_id: String,
    pub call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingReply {
    pub call_id: String,
    /// The envelope that carried this call out, which the answer names in its
    /// `reply_to`. A `call_id` is only unique within one assistant message, so a
    /// later turn can reuse one; this is minted per dispatch.
    pub call_envelope_id: String,
    /// Also the name of the peer agent
    pub tool_name: String,
    pub outcome: ToolCallOutcome,
    /// When the sub-agent call was dispatched, carried so the eventual reply's
    /// `ToolMessage` records the full execution duration.
    pub started_at: jiff::Timestamp,
}

#[derive(Debug, Clone)]
pub struct ToolExecutionState {
    /// The assistant message these calls came from. One generation produces one
    /// batch, so the whole batch shares a parent; carrying it here (rather than
    /// per call) keeps it available after an approval suspension or a process
    /// restart, when the message itself is no longer in scope.
    pub parent_message_id: MessageId,
    /// Replies waiting from stateful sub-agents.
    pub pending_replies: Vec<PendingReply>,
    pub tool_calls: VecDeque<PendingToolCall>,
}

#[derive(Debug, Clone)]
pub struct PendingToolCall {
    pub tool_call: ToolCall,
    pub outcome: ToolCallOutcome,
}

#[derive(Debug, Clone, Default)]
pub enum ResumePoint {
    #[default]
    Generation,
    ToolExecution(ToolExecutionState),
    PendingApproval {
        /// The assistant message these calls came from — see
        /// [`ToolExecutionState::parent_message_id`]. Persisted with the
        /// suspension so a sub-agent dispatched after the approval still knows
        /// what triggered it, even across a process restart.
        parent_message_id: MessageId,
        /// Tool calls waiting for approval.
        pending_approval_calls: VecDeque<ToolCall>,
        /// Tool calls to execute.
        pending_calls: VecDeque<PendingToolCall>,
    },
}

/// One message in a thread's history, tagged with the turn it belongs to.
///
/// `turn_id` sits out here rather than inside `Message` for the same reason
/// `thread_id` does: it describes where the message falls in the session's
/// control flow, not what the message says. Keeping it out also means the
/// provider adapter — which builds assistant messages and has no idea what a
/// turn is — never has to supply it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub turn_id: TurnId,
    pub message: Message,
}

pub struct AgentState {
    pub messages: Vec<HistoryEntry>,
    /// Tool state this thread has recorded, anchored to the messages that
    /// recorded it — see [`StoredCheckpoint::state`](crate::persist::StoredCheckpoint::state).
    /// Grows in step with `messages` and is cut by the same rule.
    pub state: Vec<StateEntry>,
    /// The turn newly appended messages belong to. Advances only when a user
    /// message is appended (see [`Agent::add_user_message`]); `None` only before
    /// this thread has any history at all.
    pub current_turn: Option<TurnId>,
}

/// Identifies what was interrupted by an abort.
#[derive(Debug, Clone)]
pub enum AbortedTarget {
    /// LLM generation was interrupted.
    Generation,
    /// Tool execution was interrupted; carries the IDs of unfinished tool calls.
    ToolCalls(Vec<String>),
}

#[derive(Eq, Hash, PartialEq, Clone, Debug, Serialize, Deserialize)]
pub struct ThreadId(pub(crate) String);

impl Default for ThreadId {
    fn default() -> Self {
        Self::new()
    }
}

/// Namespace for hashing a non-UUID thread id into a usable uuid5 namespace.
/// Arbitrary but fixed: changing it changes every derived thread id.
const NON_UUID_THREAD_NAMESPACE: Uuid = Uuid::from_u128(0x3f7a1c62_5be4_4d0f_9a31_c6d84b7e02f5);

impl ThreadId {
    pub fn new() -> Self {
        ThreadId(Uuid::new_v4().to_string())
    }

    /// Derive a child thread id from its parent and a name.
    ///
    /// A parent id that isn't a UUID — the root thread id is the client-supplied
    /// session id, which is only required to be a safe string — is hashed into a
    /// namespace rather than falling back to the nil one. Falling back would
    /// give every such session the *same* namespace, so two sessions would
    /// derive identical child ids and "a different parent means different
    /// children" would silently stop holding.
    pub fn from_uuid5(namespace: &ThreadId, name: &str) -> Self {
        let ns = Uuid::parse_str(&namespace.0)
            .unwrap_or_else(|_| Uuid::new_v5(&NON_UUID_THREAD_NAMESPACE, namespace.0.as_bytes()));
        ThreadId(Uuid::new_v5(&ns, name.as_bytes()).to_string())
    }
}

impl AsRef<str> for ThreadId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for ThreadId {
    fn from(s: String) -> Self {
        ThreadId(s)
    }
}

/// The sender of an envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Sender {
    /// Message from the user.
    User,
    /// Message from another agent.
    Agent { name: String, thread_id: ThreadId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receiver {
    pub name: String,
    pub thread_id: ThreadId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EnvelopeBody {
    Task {
        /// Identity for the user message this task becomes, minted once at the
        /// request boundary. The relay builds its own copy of that message for
        /// the live snapshot, so the id has to travel with the task rather than
        /// be minted here.
        message_id: MessageId,
        task: String,
        /// Base64 data-URIs or HTTPS URLs for images to attach to this turn.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    /// Call agent as a tool
    ToolCall {
        call_id: String,
        /// The assistant message in the calling thread whose tool call this is.
        /// Paired with `call_id` it forms the [`MessageOrigin`] the receiving
        /// thread stamps on its opening message; only the parent id travels
        /// here, since `call_id` is already alongside it.
        parent_message_id: MessageId,
        /// The name the caller derived the receiving thread's id from. Sent so
        /// the receiver can record how it was addressed without re-deriving it
        /// (which would mean knowing the caller's mode for it).
        derivation_key: String,
        /// The turn this call is part of. A sub-agent doesn't start a turn — it
        /// works inside the caller's — so the turn has to be handed down for its
        /// messages to group with the submission that ultimately caused them.
        turn_id: TurnId,
        task: String,
    },
    /// Reply from a agent, containing the tool output.
    Reply {
        call_id: String,
        output: ToolOutput,
        /// Whether the answering thread was interrupted rather than finishing
        /// its work. Only the answerer knows this, and the caller needs it to
        /// record the call as aborted instead of merely failed.
        aborted: bool,
    },
    Resume(ResumeDecision),
}

/// An envelope is a message delivered to an agent, containing the message body and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// A unique identifier for this message, used for tracking and replying.
    pub id: String,
    /// Sender of the message.
    pub from: Sender,
    /// Receiver of the message.
    pub to: Receiver,
    /// If this message is a reply to another message, this field contains the ID of the original message. Otherwise, it is None.
    pub reply_to: Option<String>,
    /// The content of the message.
    pub body: EnvelopeBody,
}

impl Envelope {
    pub fn with_id(f: impl FnOnce(String) -> Self) -> Self {
        f(Uuid::new_v4().to_string())
    }
}

/// Events produced by `Agent::run` and `Agent::resume`.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    LLMStart(ChatCompletionRequest),
    LLMContentChunk(String),
    /// A chunk of the model's reasoning / chain-of-thought text (reasoning
    /// models only, e.g. DeepSeek).
    LLMReasoningChunk(String),
    LLMEnd(AssistantMessage),
    ToolCallStart(ToolCall),
    ToolCallEnd(ToolMessage),
    /// Emitted when tool calls require human approval. The agent thread exits
    /// after this event. The caller should shut down the session, collect
    /// decisions, and open a new session with `resume_decisions` to continue.
    Suspended(PendingApproval),
    /// Emitted when the run is aborted by the user. The stream terminates after this event.
    Aborted(AbortedTarget),
    Error(String), // TODO: make this more structured
    /// This turn's content could not be written to the database. **Not** a
    /// turn-ending event: whoever receives it must not treat the turn as
    /// finished, because what is on screen is not what is stored.
    PersistFailed(String),
}

/// Produces the template-variable bindings for a turn — the `{{name}}` values
/// substituted into the assembled system prompt (date, os, shell, workspace, …).
/// Invoked fresh at the start of every turn so volatile values — the date above
/// all — are never stale. The closure captures the agent's workspace directory
/// and computes the static values once; only truly volatile values are
/// recomputed per call (see the provider constructed in `coda_server`).
pub type VarsProvider = Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>;

/// Substitute `{{ name }}` placeholders in `template` with values from `vars`
/// (optional inner whitespace allowed; names are `[A-Za-z0-9_]`). Anything that
/// isn't a resolvable placeholder — an unknown name, a malformed span, or a
/// stray `{{` — is emitted verbatim. Substitution is single-pass: a value that
/// itself contains `{{…}}` is not re-scanned, so bindings can't inject further
/// placeholders.
pub fn substitute(template: &str, vars: &[(String, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let mut resolved = false;
        if let Some(close) = after.find("}}") {
            let name = after[..close].trim();
            if !name.is_empty()
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && let Some((_, value)) = vars.iter().find(|(var, _)| var == name)
            {
                out.push_str(value);
                rest = &after[close + 2..];
                resolved = true;
            }
        }
        if !resolved {
            // Not a resolvable placeholder: emit the literal `{{` and keep
            // scanning from just after it, so later braces still get a chance.
            out.push_str("{{");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The system prompt an agent prepends to its messages at the start of every
/// turn. There is a single template — the `base` body — plus the per-turn
/// variables substituted into it:
///
/// - `base` — the agent's own body (built-in default or `AGENT.md` body), the
///   one and only template. Held behind a handle so it *can* be updated in place
///   without rebuilding the agent, though the server currently sets it once at
///   load.
/// - `vars` — the per-turn template-variable bindings (date, os, shell,
///   workspace, skills, workspace custom instructions, …), produced fresh every
///   turn so the date and other volatile values stay current. Everything
///   dynamic — the environment context, the skills guide and list, and the
///   workspace's `AGENTS.md` — is a `{{name}}` binding the base body composes.
///   Because substitution is single-pass, a binding's value (e.g. `AGENTS.md`
///   or a skill description) is never re-scanned, so authored content is not
///   itself treated as a template.
///
/// [`resolve`](Self::resolve) substitutes the variables into the base each turn.
#[derive(Clone)]
pub struct SystemPrompt {
    base: SharedSystemPrompt,
    vars: Option<VarsProvider>,
}

impl SystemPrompt {
    /// A prompt with only a base body — no variables.
    pub fn new(base: SharedSystemPrompt) -> Self {
        SystemPrompt { base, vars: None }
    }

    /// Attach the per-turn template-variable provider. Its bindings are
    /// substituted into the base body each turn.
    pub fn with_vars(mut self, vars: VarsProvider) -> Self {
        self.vars = Some(vars);
        self
    }

    /// The current prompt text: the base body with the per-turn variables
    /// substituted into it. Unknown `{{placeholders}}` are left untouched, and a
    /// binding's value is never re-scanned (single pass).
    pub fn resolve(&self) -> String {
        let base = self.base.get();
        match &self.vars {
            Some(vars) => substitute(&base, &vars()),
            None => base,
        }
    }
}

impl From<&str> for SystemPrompt {
    fn from(s: &str) -> Self {
        SystemPrompt::new(SharedSystemPrompt::new(s))
    }
}

impl From<String> for SystemPrompt {
    fn from(s: String) -> Self {
        SystemPrompt::new(SharedSystemPrompt::new(s))
    }
}

impl From<SharedSystemPrompt> for SystemPrompt {
    fn from(s: SharedSystemPrompt) -> Self {
        SystemPrompt::new(s)
    }
}

/// A mutable, shareable system prompt. Clones share the same storage; a `set`
/// from any holder is observed by every agent built from it on their next turn.
#[derive(Clone)]
pub struct SharedSystemPrompt(Arc<std::sync::RwLock<String>>);

impl SharedSystemPrompt {
    pub fn new(prompt: impl Into<String>) -> Self {
        SharedSystemPrompt(Arc::new(std::sync::RwLock::new(prompt.into())))
    }

    pub fn set(&self, prompt: impl Into<String>) {
        *self.0.write().unwrap() = prompt.into();
    }

    pub fn get(&self) -> String {
        self.0.read().unwrap().clone()
    }
}

pub struct Agent {
    pub name: String,
    pub mode: SubAgentMode,
    pub system_prompt: SystemPrompt,
    pub state: Arc<Mutex<AgentState>>,
    pub tools: Tools,
    pub subagents: SubAgents,
}

/// A model and its sampling parameters. One agent runs on exactly one profile
/// per turn; a session can map different agents to different profiles through
/// [`RunConfig::agent_models`].
pub struct ModelProfile<P> {
    pub provider: P,
    pub model: String,
    /// Human-readable identifier for logging (the `provider_id:model_id`
    /// selection key). Distinct from `model`, which is the bare API model name.
    pub label: String,
    pub temperature: Option<f32>,
    pub max_completion_tokens: Option<u32>,
    /// Reasoning effort sent on each generation request. `None` leaves the
    /// provider default untouched; `Some("off")` turns thinking off.
    pub reasoning_effort: Option<String>,
    /// The token count at which the root thread automatically compacts
    /// context mid-turn. Already resolved to a concrete value by the caller —
    /// this type carries no default policy of its own.
    pub auto_compact_threshold_tokens: u32,
}

impl<P: Clone> Clone for ModelProfile<P> {
    fn clone(&self) -> Self {
        ModelProfile {
            provider: self.provider.clone(),
            model: self.model.clone(),
            label: self.label.clone(),
            temperature: self.temperature,
            max_completion_tokens: self.max_completion_tokens,
            reasoning_effort: self.reasoning_effort.clone(),
            auto_compact_threshold_tokens: self.auto_compact_threshold_tokens,
        }
    }
}

/// Per-session run configuration. Every agent shares the same tool-approval
/// policy, but each can run on its own [`ModelProfile`]: the root agent — and
/// any agent without an entry in `agent_models` — uses `default_model`, while
/// `agent_models` overrides specific agents by name.
pub struct RunConfig<P> {
    pub default_model: ModelProfile<P>,
    /// Per-agent model overrides, keyed by agent name. Agents absent here fall
    /// back to `default_model`.
    pub agent_models: HashMap<String, ModelProfile<P>>,
    pub tool_approval: ToolApprovalMode,
    /// If set, pending approvals older than this duration are auto-rejected
    /// when opening a session.
    pub approval_timeout: Option<std::time::Duration>,
}

impl<P: Clone> RunConfig<P> {
    /// Resolve the configuration for a single agent: its model override if one is
    /// registered, otherwise `default_model`, paired with the shared approval mode.
    pub(crate) fn resolve(&self, agent_name: &str) -> AgentRunConfig<P> {
        let profile = self
            .agent_models
            .get(agent_name)
            .cloned()
            .unwrap_or_else(|| self.default_model.clone());
        AgentRunConfig {
            profile,
            tool_approval: self.tool_approval.clone(),
        }
    }
}

impl<P: Clone> Clone for RunConfig<P> {
    fn clone(&self) -> Self {
        RunConfig {
            default_model: self.default_model.clone(),
            agent_models: self.agent_models.clone(),
            tool_approval: self.tool_approval.clone(),
            approval_timeout: self.approval_timeout,
        }
    }
}

/// The resolved configuration handed to a single agent's run loop.
#[derive(Clone)]
pub(crate) struct AgentRunConfig<P> {
    pub profile: ModelProfile<P>,
    pub tool_approval: ToolApprovalMode,
}

impl Agent {
    pub fn state(&self) -> Arc<Mutex<AgentState>> {
        self.state.clone()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Append a user message and make its turn this thread's current one, in a
    /// single critical section.
    ///
    /// Advancing the turn is deliberately tied to appending the user message
    /// rather than to receiving an envelope. When a new task pre-empts calls that
    /// were awaiting approval, the driver first writes those calls off as aborted
    /// `ToolMessage`s; those results belong to the *previous* turn. Were the turn
    /// advanced on arrival, a rewind to the new turn would delete them and leave
    /// the earlier assistant message with tool calls that have no results —
    /// history the provider rejects. Advancing here lets them keep the old turn.
    pub async fn add_user_message(&self, turn_id: TurnId, message: UserMessage) {
        debug!("Adding user message: {:?}", message);
        let mut state = self.state.lock().await;
        state.current_turn = Some(turn_id);
        state.messages.push(HistoryEntry {
            turn_id,
            message: Message::User(message),
        });
    }

    /// Append a message to the current turn. Used for assistant and tool
    /// messages, which never start a turn.
    pub async fn add_message(&self, message: Message) {
        debug!("Adding message: {:?}", message);
        let mut state = self.state.lock().await;
        let turn_id = state.stamp();
        state.messages.push(HistoryEntry { turn_id, message });
    }

    pub async fn add_messages(&self, messages: Vec<Message>) {
        debug!("Adding messages: {:?}", messages);
        let mut state = self.state.lock().await;
        let turn_id = state.stamp();
        state.messages.extend(
            messages
                .into_iter()
                .map(|message| HistoryEntry { turn_id, message }),
        );
    }

    /// The turn this thread is in: what a message appended now is tagged with,
    /// and what a sub-agent call hands down so the callee's messages group with
    /// the submission that ultimately caused them.
    ///
    /// `None` while the thread has no history — a thread opened but not yet
    /// prompted is in no turn, and asking is not an error. Callers on that path
    /// (the driver entering a fresh thread) supply the turn they were entered
    /// with instead. Read-only, deliberately: this used to go through
    /// [`AgentState::stamp`], so merely *asking* on a fresh thread minted a
    /// throwaway turn and logged its invariant break.
    pub async fn current_turn(&self) -> Option<TurnId> {
        self.state.lock().await.current_turn
    }

    /// The request this thread's history makes: the system prompt, then the
    /// part of history a compaction left visible, lowered to what a provider
    /// accepts.
    pub async fn messages(&self) -> Vec<RequestMessage> {
        let history = self.state.lock().await;
        let visible = message_view::model_view(&history.messages);
        let mut messages = Vec::with_capacity(history.messages.len() + 1);
        messages.push(RequestMessage::System(SystemMessage(
            self.system_prompt.resolve(),
        )));
        messages.extend(visible.filter_map(|entry| (&entry.message).into()));
        messages
    }

    /// Returns conversation history without the system prompt (suitable for checkpointing).
    pub async fn history(&self) -> Vec<HistoryEntry> {
        self.state.lock().await.messages.clone()
    }

    /// The most recent recorded token usage on this thread, read without
    /// cloning the transcript — the cheap check to run before deciding
    /// whether [`Agent::history`]'s full clone is worth paying for.
    pub async fn last_usage(&self) -> Option<CompletionUsage> {
        let state = self.state.lock().await;
        state
            .messages
            .iter()
            .rev()
            .find_map(|entry| match &entry.message {
                Message::Assistant(assistant) => assistant.usage.clone(),
                _ => None,
            })
    }

    /// Restore a stored thread's conversation and anchored tool state, replacing
    /// whatever thread this agent last ran. State remains opaque here; tools
    /// interpret their own kinds through `ToolCallContext::state` when invoked.
    pub async fn restore_history(&self, messages: Vec<HistoryEntry>, entries: Vec<StateEntry>) {
        let mut state = self.state.lock().await;
        state.messages = messages;
        state.state = entries;
        // Whatever work is being resumed belongs to the turn of the last message
        // written, so the turn needs no separate persistence.
        state.current_turn = state.messages.last().map(|entry| entry.turn_id);
    }

    /// This thread's recorded state so far, reduced to one value per kind.
    ///
    /// Last-wins, because every entry is a complete value rather than a delta —
    /// the property that also lets a compaction collapse a range of entries
    /// without knowing what any kind means.
    pub async fn state_snapshot(&self) -> HashMap<String, serde_json::Value> {
        let state = self.state.lock().await;
        let mut snapshot = HashMap::new();
        for entry in &state.state {
            snapshot.insert(entry.kind.clone(), entry.value.clone());
        }
        snapshot
    }

    pub async fn state_entries(&self) -> Vec<StateEntry> {
        self.state.lock().await.state.clone()
    }

    /// Append a message together with whatever the call that produced it
    /// recorded. One critical section, because an entry without its anchor is
    /// state nothing can cut and an anchor without its entry silently loses a
    /// write.
    pub async fn add_message_with_state(
        &self,
        message: Message,
        recorded: Vec<(String, serde_json::Value)>,
    ) {
        let anchor = message.message_id();
        let mut state = self.state.lock().await;
        let turn_id = state.stamp();
        state.messages.push(HistoryEntry { turn_id, message });
        state
            .state
            .extend(recorded.into_iter().map(|(kind, value)| StateEntry {
                message_id: anchor,
                kind,
                value,
            }));
    }
}

impl AgentState {
    /// The turn to tag a newly appended assistant/tool message with. Only the
    /// append paths call this — reading the turn goes through
    /// [`Agent::current_turn`], which reports "no turn yet" rather than minting
    /// one, so the error below stays a report of a real invariant break.
    ///
    /// `current_turn` is `None` only before a thread has any history, and an
    /// assistant or tool message can't be the first thing in a thread — one
    /// always follows the user message that prompted it. Should that ever break,
    /// keeping the message under a fresh turn beats dropping it: a mis-grouped
    /// message is a rewind inaccuracy, a missing tool result is history the
    /// provider refuses outright.
    fn stamp(&mut self) -> TurnId {
        match self.current_turn {
            Some(turn_id) => turn_id,
            None => {
                error!("appending to a thread with no current turn; tagging a fresh one");
                let turn_id = TurnId::from(MessageId::new());
                self.current_turn = Some(turn_id);
                turn_id
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubAgentMode {
    Stateless,
    Stateful,
}

pub struct SubAgentTool {
    pub name: String,
    pub description: String,
    pub mode: SubAgentMode,
}

#[derive(Clone, Default)]
pub struct SubAgents(Vec<Arc<SubAgentTool>>);

impl SubAgents {
    pub fn register(&mut self, subagent: SubAgentTool) {
        self.0.push(Arc::new(subagent));
    }

    /// Resolve a sub-agent by its tool name. Accepts both the prefixed name the
    /// LLM sees (`agent__foo`) and the bare agent name (`foo`).
    pub fn get(&self, name: &str) -> Option<Arc<SubAgentTool>> {
        let bare = name.strip_prefix(SUBAGENT_TOOL_PREFIX).unwrap_or(name);
        self.0.iter().find(|agent| agent.name == bare).cloned()
    }

    pub fn descriptors(&self) -> Vec<ToolDefinition> {
        self.0
            .iter()
            .map(|subagent| ToolDefinition {
                name: format!("{SUBAGENT_TOOL_PREFIX}{}", subagent.name),
                description: if subagent.mode == SubAgentMode::Stateful {
                    format!(
                        "{}\n\nIMPORTANT: This sub-agent does NOT support concurrent invocation. Do NOT call this tool more than once in the same tool-call batch. If you need to invoke it multiple times, call it sequentially — one at a time.",
                        subagent.description
                    )
                } else {
                    subagent.description.to_string()
                },
                parameter_schema: json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "The task to delegate to the sub-agent.",
                        },
                    },
                    "required": ["task"],
                }),
            })
            .collect()
    }
}

#[cfg(test)]
mod thread_id_tests {
    use super::*;

    /// The root thread id is whatever session id the client chose, and it is not
    /// required to be a UUID — the web client falls back to a non-UUID form
    /// whenever `crypto.randomUUID` is unavailable, which is every plain-HTTP
    /// origin. Two such sessions must still derive distinct child threads.
    #[test]
    fn non_uuid_parents_derive_distinct_children() {
        let one = ThreadId::from("session-mf3k2x".to_string());
        let other = ThreadId::from("session-mf3k2y".to_string());

        assert_ne!(
            ThreadId::from_uuid5(&one, "explore"),
            ThreadId::from_uuid5(&other, "explore")
        );
    }

    /// Deriving from a parent that *is* a UUID must keep using it as the
    /// namespace directly, so existing stateful thread ids are unaffected by the
    /// non-UUID handling above.
    #[test]
    fn uuid_parent_is_used_as_the_namespace_directly() {
        let parent = ThreadId::from("6ba7b810-9dad-11d1-80b4-00c04fd430c8".to_string());

        assert_eq!(
            ThreadId::from_uuid5(&parent, "explore").as_ref(),
            Uuid::new_v5(&Uuid::parse_str(parent.as_ref()).unwrap(), b"explore").to_string()
        );
    }
}

#[cfg(test)]
mod system_prompt_tests {
    use super::*;

    #[test]
    fn resolve_base_only() {
        let sp = SystemPrompt::from("base body");
        assert_eq!(sp.resolve(), "base body");
    }

    #[test]
    fn resolve_composes_everything_from_vars_including_authored_content() {
        // The base body places both the env date and the workspace's AGENTS.md
        // (as a variable). The AGENTS.md value contains `{{date}}` but, being a
        // binding value, is not re-scanned — it stays verbatim.
        let vars: VarsProvider = Arc::new(|| {
            vec![
                ("date".into(), "2026-07-24".into()),
                (
                    "workspace_custom_instructions".into(),
                    "be concise. today is {{date}}".into(),
                ),
            ]
        });
        let sp = SystemPrompt::new(SharedSystemPrompt::new(
            "today: {{date}}\n---\n{{workspace_custom_instructions}}",
        ))
        .with_vars(vars);
        assert_eq!(
            sp.resolve(),
            "today: 2026-07-24\n---\nbe concise. today is {{date}}"
        );
    }

    #[test]
    fn resolve_injects_empty_string_for_empty_bindings() {
        let vars: VarsProvider = Arc::new(|| vec![("available_skills".into(), String::new())]);
        let sp =
            SystemPrompt::new(SharedSystemPrompt::new("root{{available_skills}}")).with_vars(vars);
        assert_eq!(sp.resolve(), "root");
    }

    #[test]
    fn substitute_leaves_unknown_and_malformed_placeholders_untouched() {
        let vars = [("date".to_string(), "2026-07-23".to_string())];
        // Unknown name, no closing braces, and a non-name span all pass through.
        assert_eq!(
            substitute("{{date}} {{unknown}} {{ a b }} {{oops", &vars),
            "2026-07-23 {{unknown}} {{ a b }} {{oops"
        );
    }

    #[test]
    fn substitute_does_not_rescan_values() {
        // A value containing `{{date}}` must not be expanded again.
        let vars = [("x".to_string(), "{{date}}".to_string())];
        assert_eq!(substitute("{{x}}", &vars), "{{date}}");
    }

    #[test]
    fn resolve_reflects_binding_handle_updates_in_place() {
        // A binding sourced from a shared handle (as the server wires skills /
        // custom instructions) reflects in-place updates on the next resolve —
        // this is how workspace-knowledge hot-reload reaches the prompt now.
        let handle = SharedSystemPrompt::new("old");
        let vars: VarsProvider = {
            let handle = handle.clone();
            Arc::new(move || vec![("available_skills".into(), handle.get())])
        };
        let sp = SystemPrompt::new(SharedSystemPrompt::new("skills: {{available_skills}}"))
            .with_vars(vars);
        assert_eq!(sp.resolve(), "skills: old");
        handle.set("new");
        assert_eq!(sp.resolve(), "skills: new");
    }
}
