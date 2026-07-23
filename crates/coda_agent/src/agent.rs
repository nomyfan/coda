use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, Message, SystemMessage, ToolCall, ToolCallOutcome,
    ToolDefinition, ToolMessage, ToolOutput,
};
use coda_core::tool::Tools;
use coda_tools::TodoItem;
use tracing::debug;

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeDecision {
    pub resolutions: Vec<(String, ToolCallResolution)>,
}

/// Lightweight view of an agent thread waiting for approval.
///
/// This is the public-facing type returned via [`AgentEvent::Suspended`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub thread_id: String,
    pub agent_name: String,
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
    /// Also the name of the peer agent
    pub tool_name: String,
    pub outcome: ToolCallOutcome,
    /// When the sub-agent call was dispatched, carried so the eventual reply's
    /// `ToolMessage` records the full execution duration.
    pub started_at: jiff::Timestamp,
}

#[derive(Debug, Clone)]
pub struct ToolExecutionState {
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
        /// Tool calls waiting for approval.
        pending_approval_calls: VecDeque<ToolCall>,
        /// Tool calls to execute.
        pending_calls: VecDeque<PendingToolCall>,
    },
}

pub struct AgentState {
    pub messages: Vec<Message>,
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

impl ThreadId {
    pub fn new() -> Self {
        ThreadId(Uuid::new_v4().to_string())
    }

    pub fn from_uuid5(namespace: &ThreadId, name: &str) -> Self {
        let ns = Uuid::parse_str(&namespace.0).unwrap_or(Uuid::nil());
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
        task: String,
        /// Base64 data-URIs or HTTPS URLs for images to attach to this turn.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    /// Call agent as a tool
    ToolCall {
        call_id: String,
        task: String,
    },
    /// Reply from a agent, containing the tool output.
    Reply {
        call_id: String,
        output: ToolOutput,
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
    pub todo_store: Arc<Mutex<Vec<TodoItem>>>,
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

    pub async fn add_message(&self, message: Message) {
        debug!("Adding message: {:?}", message);
        self.state.lock().await.messages.push(message);
    }

    pub async fn add_messages(&self, messages: Vec<Message>) {
        debug!("Adding messages: {:?}", messages);
        self.state.lock().await.messages.extend(messages);
    }

    pub async fn todos(&self) -> Vec<TodoItem> {
        self.todo_store.lock().await.clone()
    }

    pub async fn messages(&self) -> Vec<Message> {
        let mut messages = self.state.lock().await.messages.clone();
        messages.insert(
            0,
            Message::System(SystemMessage(self.system_prompt.resolve())),
        );
        messages
    }

    /// Returns conversation history without the system prompt (suitable for checkpointing).
    pub async fn history(&self) -> Vec<Message> {
        self.state.lock().await.messages.clone()
    }

    pub async fn restore_history(&self, messages: Vec<Message>, todos: Vec<TodoItem>) {
        let mut state = self.state.lock().await;
        // Filter out any SystemMessage that may have been saved in old checkpoints.
        state.messages = messages
            .into_iter()
            .filter(|m| !matches!(m, Message::System(_)))
            .collect();
        *self.todo_store.lock().await = todos;
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
