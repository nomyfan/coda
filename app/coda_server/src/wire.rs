use crate::config::{PermissionMode, ToolApprovalConfig, extract_shell_command};
use crate::files::FileEntry;
use coda_agent::{AbortedTarget, AgentEvent, EventOrigin, ResumeDecision, SessionEvent};
use coda_core::llm::{
    AssistantMessage, CompactionMessage, Message, MessageId, Modality, ToolCall, ToolMessage,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WireEvent {
    #[serde(rename = "llm_start")]
    LlmStart {
        agent_name: String,
        thread_id: String,
        model: String,
    },
    #[serde(rename = "llm_chunk")]
    LlmContentChunk {
        agent_name: String,
        thread_id: String,
        content: String,
    },
    #[serde(rename = "llm_reasoning_chunk")]
    LlmReasoningChunk {
        agent_name: String,
        thread_id: String,
        content: String,
    },
    #[serde(rename = "llm_end")]
    LlmEnd {
        agent_name: String,
        thread_id: String,
        message: AssistantMessage,
    },
    #[serde(rename = "tool_start")]
    ToolCallStart {
        agent_name: String,
        thread_id: String,
        call: ToolCall,
    },
    #[serde(rename = "tool_end")]
    ToolCallEnd {
        agent_name: String,
        thread_id: String,
        message: ToolMessage,
    },
    /// An auto-compaction has begun. Live-only, like `tool_start`.
    #[serde(rename = "compaction_start")]
    CompactionStart {
        agent_name: String,
        thread_id: String,
    },
    /// An auto-compaction's outcome: the summary it wrote, or the record of
    /// why it wrote none.
    #[serde(rename = "compaction_end")]
    CompactionEnd {
        agent_name: String,
        thread_id: String,
        message: CompactionMessage,
    },
    #[serde(rename = "suspended")]
    Suspended {
        agent_name: String,
        thread_id: String,
        approval: PendingApprovalWire,
    },
    #[serde(rename = "aborted")]
    Aborted {
        agent_name: String,
        thread_id: String,
        target: AbortedTargetWire,
    },
    #[serde(rename = "error")]
    Error {
        agent_name: String,
        thread_id: String,
        message: String,
    },
    /// This turn's content could not be written to the database. Deliberately
    /// not a turn-ending event: the client must not show the turn as finished,
    /// because what is on screen is not what is stored.
    #[serde(rename = "persist_failed")]
    PersistFailed {
        agent_name: String,
        thread_id: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reason")]
pub enum AbortedTargetWire {
    #[serde(rename = "generation")]
    Generation,
    #[serde(rename = "tool_calls")]
    ToolCalls { call_ids: Vec<String> },
}

impl From<AbortedTarget> for AbortedTargetWire {
    fn from(t: AbortedTarget) -> Self {
        match t {
            AbortedTarget::Generation => AbortedTargetWire::Generation,
            AbortedTarget::ToolCalls(ids) => AbortedTargetWire::ToolCalls { call_ids: ids },
        }
    }
}

impl WireEvent {
    pub fn from_session_event(event: SessionEvent, root_name: &str) -> Self {
        let agent_name = match &event.origin {
            EventOrigin::Root => root_name.to_string(),
            EventOrigin::Sub { name } => name.clone(),
        };
        let thread_id = event.thread_id.as_ref().to_string();

        match event.kind {
            AgentEvent::LLMStart(request) => WireEvent::LlmStart {
                agent_name,
                thread_id,
                model: request.model,
            },
            AgentEvent::LLMContentChunk(content) => WireEvent::LlmContentChunk {
                agent_name,
                thread_id,
                content,
            },
            AgentEvent::LLMReasoningChunk(content) => WireEvent::LlmReasoningChunk {
                agent_name,
                thread_id,
                content,
            },
            AgentEvent::LLMEnd(message) => WireEvent::LlmEnd {
                agent_name,
                thread_id,
                message,
            },
            AgentEvent::ToolCallStart(call) => WireEvent::ToolCallStart {
                agent_name,
                thread_id,
                call,
            },
            AgentEvent::ToolCallEnd(message) => WireEvent::ToolCallEnd {
                agent_name,
                thread_id,
                message,
            },
            AgentEvent::CompactionStart => WireEvent::CompactionStart {
                agent_name,
                thread_id,
            },
            AgentEvent::CompactionEnd(message) => WireEvent::CompactionEnd {
                agent_name,
                thread_id,
                message,
            },
            AgentEvent::Suspended(approval) => WireEvent::Suspended {
                agent_name,
                thread_id,
                approval: PendingApprovalWire::from_agent(approval),
            },
            AgentEvent::Aborted(target) => WireEvent::Aborted {
                agent_name,
                thread_id,
                target: target.into(),
            },
            AgentEvent::Error(message) => WireEvent::Error {
                agent_name,
                thread_id,
                message,
            },
            AgentEvent::PersistFailed(message) => WireEvent::PersistFailed {
                agent_name,
                thread_id,
                message,
            },
        }
    }
}

// --- Request params (client→server) ------------------------------------------
//
// `list_workspaces` and `list_providers` carry no params. Each remaining method
// deserializes its `params` object into one of these. Fields mirror the former
// `ClientMessage` variants one-for-one.

/// `open_session` params. `provider_id`/`reasoning_effort` carry a client-chosen
/// selection (e.g. picked on a new session before the first message); both
/// default to the server's defaults when omitted. `takeover` evicts whoever
/// currently holds the session — an explicit user decision; without it a held
/// session is refused with the `SESSION_BUSY` error.
///
/// `permission_mode` is the posture the client remembers for this session. It
/// seeds a session the server is not already running; a live one keeps its own
/// and reports it in the [`Snapshot`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenSessionParams {
    pub workspace_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub takeover: bool,
}

/// `task` params — start a new turn, optionally with image attachments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskParams {
    pub workspace_id: String,
    pub session_id: String,
    pub task: String,
    /// Base64 data-URIs (`data:image/<fmt>;base64,<b64>`) or HTTPS URLs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

/// `rewind` params — discard `message_id` and everything the session produced
/// from it onward, then start a fresh turn from the edited text.
///
/// `task`/`images` carry the edited message and go through exactly the same
/// checks as [`TaskParams`]. `message_id` is the only identity the client
/// supplies; it must name a user message of this session's root thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindParams {
    pub workspace_id: String,
    pub session_id: String,
    pub message_id: MessageId,
    pub task: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

/// `resume` params — answer a suspended tool call. `agent_name`/`thread_id` come
/// from the [`PendingApprovalWire`] carried by a `Suspended` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeParams {
    pub workspace_id: String,
    pub session_id: String,
    pub agent_name: String,
    pub thread_id: String,
    pub decision: ResumeDecision,
}

/// `abort` / `close_session` params — both identify only a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRef {
    pub workspace_id: String,
    pub session_id: String,
}

/// `delete_session` params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteSessionParams {
    pub workspace_id: String,
    pub session_id: String,
}

/// `fork_session` params. `cut_message_id` names a user message of the source's
/// root thread, and the copy keeps the turns before the one it opened; omitting
/// it copies everything stored. The new session id is minted by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkSessionParams {
    pub workspace_id: String,
    pub session_id: String,
    #[serde(default)]
    pub cut_message_id: Option<MessageId>,
}

/// Result of `fork_session`: the session that was minted, plus a refreshed
/// catalog so the list shows it without a second round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkAccepted {
    pub session_id: String,
    pub name: Option<String>,
    pub workspaces: Vec<WorkspaceSummaryWire>,
}

/// `rename_session` params. `null` or a blank name clears the custom name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameSessionParams {
    pub workspace_id: String,
    pub session_id: String,
    pub name: Option<String>,
}

/// `add_allow_pattern` params — append a glob to the shell allow-list; takes
/// effect immediately for the live session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddAllowPatternParams {
    pub workspace_id: String,
    pub pattern: String,
}

/// `list_files` params — the composer's `@` picker searching one workspace.
/// `query` is the text typed after the `@` (empty right after it), and `limit`
/// caps the menu; both have server-side defaults so a bare workspace id works.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListFilesParams {
    pub workspace_id: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `list_skills` params — the composer's `/` picker reading one workspace's
/// `.coda/skills`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListSkillsParams {
    pub workspace_id: String,
}

/// `set_model` params. An opened session rejects a different provider/model;
/// the same model may update its reasoning setting while idle, applied from the
/// next turn by reopening the runtime. `null` selects the first configured
/// effort, `off` turns thinking off, and models without controls keep `null`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetModelParams {
    pub workspace_id: String,
    pub session_id: String,
    pub provider_id: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

// --- Result / server-push payloads -------------------------------------------
//
// These serialize identically whether framed as a request `result` or a
// notification `params`, so the same struct backs both the solicited and the
// unsolicited path (see Load-Bearing Decision 5).

/// Result of `list_workspaces` / `delete_session`, and (historically) a
/// `workspace_catalog` push: the configured workspaces and their sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceCatalog {
    pub workspaces: Vec<WorkspaceSummaryWire>,
}

/// Result of `list_providers`: the models the dashboard can choose between and
/// the one new sessions default to. Static for the server's lifetime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCatalog {
    pub providers: Vec<ProviderInfoWire>,
    pub default_provider: String,
}

/// Result of `list_files`: the ranked matches for one picker query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCatalog {
    pub files: Vec<FileEntry>,
    /// Matches were left out — the menu is a page, not the whole workspace, and
    /// typing more of the query is what narrows it.
    pub truncated: bool,
}

/// Result of `list_skills`: the workspace's skills, named and described exactly
/// as the model sees them in `<available_skills>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCatalog {
    pub skills: Vec<SkillInfoWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInfoWire {
    pub name: String,
    pub description: String,
}

/// Result of `set_model`: the selection now in effect (echoed on a real switch
/// and on an idempotent no-op).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSelection {
    pub provider_id: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// `set_permission_mode` params — change how much the session may do
/// unattended. Accepted whatever the session is doing: it rebuilds nothing, and
/// applies from the next tool call rather than to calls already suspended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetPermissionModeParams {
    pub workspace_id: String,
    pub session_id: String,
    pub mode: PermissionMode,
}

/// Result of `set_permission_mode`: the mode now in effect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionModeSelection {
    pub mode: PermissionMode,
}

/// Result of `rename_session`: the normalized name persisted by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionName {
    pub name: Option<String>,
}

/// Result of `task`: the id the server minted for the user message the task
/// became. `task` is a request rather than a notification precisely so this id
/// can come back — the client renders the message optimistically and then keys
/// it on this id, so both sides name the same message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAccepted {
    pub message_id: MessageId,
}

/// Result of `rewind`: the id minted for the edited message, and the history
/// that survived the truncation — *without* that message.
///
/// The client rebuilds its transcript from `messages` and then appends the
/// edited message itself, keyed on `message_id`. It has to append it: the event
/// stream never carries user messages, so a client that only applied `messages`
/// would show the following assistant output hanging off the old history with
/// nothing to explain it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindAccepted {
    pub message_id: MessageId,
    pub messages: Vec<Message>,
}

/// Result of `open_session`, and the payload of an unsolicited `snapshot`
/// notification (hub re-attach): the resumed conversation history plus any
/// approvals left pending from a prior suspension, which the client must answer
/// with `resume` before the session resumes. `provider_id`/`reasoning_effort`
/// are the session's current model selection. `turn_running` tells the client a
/// turn is still in flight — its events are replayed (then streamed) right after.
///
/// `permission_mode` is authoritative in the same way: a client attaching to a
/// session that is already running adopts what it finds here, which is how a
/// reconnect (or a takeover from another device) shows the posture the session
/// is really executing under rather than the one this browser remembered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub workspace_id: String,
    pub session_id: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub pending_approvals: Vec<PendingApprovalWire>,
    pub provider_id: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub turn_running: bool,
    /// A compaction is in flight. It runs outside the turn machinery and emits
    /// no events, so this is the only way an attaching client learns the
    /// session is busy.
    #[serde(default)]
    pub compacting: bool,
    /// Every background task the session still knows about. Carried here as
    /// well as pushed, because a task can be started (and finish) long before
    /// this client attached.
    #[serde(default)]
    pub background_tasks: Vec<TaskSummaryWire>,
}

/// One background task, as the dashboard lists it. `status` is the humanized
/// label; `running` is what the UI groups and sorts on, so a client never has
/// to parse the label.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummaryWire {
    pub id: String,
    pub command: String,
    pub description: String,
    /// Which agent started it — a sub-agent's tasks are listed too.
    pub agent_name: String,
    pub status: String,
    pub running: bool,
    pub started_at: String,
}

impl From<coda_background::TaskSummary> for TaskSummaryWire {
    fn from(summary: coda_background::TaskSummary) -> Self {
        Self {
            id: summary.id,
            command: summary.command,
            description: summary.description,
            agent_name: summary.agent_name,
            status: summary.status.describe(),
            running: summary.status.is_running(),
            started_at: summary.started_at.to_string(),
        }
    }
}

/// Push payload for a session's background task list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTasksPush {
    pub workspace_id: String,
    pub session_id: String,
    pub tasks: Vec<TaskSummaryWire>,
}

/// Params of `compact`. `instructions` is what the user typed after
/// `/compact` — free text, shown to the summarizing model and recorded in the
/// transcript. Empty means a plain `/compact`.
#[derive(Debug, Clone, Deserialize)]
pub struct CompactParams {
    pub workspace_id: String,
    pub session_id: String,
    #[serde(default)]
    pub instructions: String,
}

/// Result of `compact`. Deliberately carries no messages: the two it wrote
/// arrive in the snapshot pushed alongside, so only one path ever writes the
/// client's transcript.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum CompactResult {
    /// The summary was written and later turns are built from it.
    Applied,
    /// No summary could be generated. What happened is recorded in the
    /// transcript — which is where the client reads it — but the conversation
    /// the model sees is unchanged.
    Recorded,
    /// Nothing was written at all — the client can retry as it stands.
    /// `stale` distinguishes "the conversation moved on" from a write failure.
    Abandoned { stale: bool, reason: String },
    /// The root thread has no history to summarize.
    Empty,
}

/// Params of an `event` notification: one live runtime event. Nested under
/// `event` so the inner `type` tag of [`WireEvent`] does not collide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventParams {
    pub workspace_id: String,
    pub session_id: String,
    pub event: WireEvent,
}

/// A model the dashboard can pick, grouped under a provider. `reasoning_efforts`
/// lists the effort levels the model offers; empty means it has no reasoning
/// controls. `input_modalities` lists the input kinds the model accepts (always
/// includes `text`; `image` enables image attachments).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfoWire {
    pub id: String,
    /// The id of the provider this model belongs to (e.g. "deepseek").
    pub provider: String,
    pub model: String,
    pub context_window: u32,
    pub reasoning_efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub input_modalities: Vec<Modality>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummaryWire {
    pub id: String,
    pub path: String,
    pub sessions: Vec<SessionSummaryWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummaryWire {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
    #[serde(default)]
    pub first_user_message: Option<String>,
    #[serde(default)]
    pub has_pending_approval: bool,
    /// `"running"` (in flight, read live from the hub), `"completed"` /
    /// `"failed"` (see [`crate::storage::UnseenOutcome`]), or absent/`null`.
    /// Computed in `workspace_catalog`, not a passthrough of one column.
    #[serde(default)]
    pub status: Option<String>,
}

/// Pushed live when a session's `unseen_outcome` changes. See
/// [`crate::hub::SessionStatusEvent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatusWire {
    pub workspace_id: String,
    pub session_id: String,
    pub outcome: String,
}

impl From<crate::hub::SessionStatusEvent> for SessionStatusWire {
    fn from(event: crate::hub::SessionStatusEvent) -> Self {
        Self {
            workspace_id: event.workspace_id,
            session_id: event.session_id,
            outcome: event.outcome.as_str().to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApprovalWire {
    pub thread_id: String,
    pub agent_name: String,
    /// Identifies the batch; the client echoes it back in `resume` so a stale
    /// decision can be told apart from a live one.
    pub parent_message_id: MessageId,
    pub calls: Vec<ToolCall>,
    pub suspended_at: jiff::Timestamp,
    pub suggested_shell_allow_patterns: BTreeMap<String, String>,
}

impl PendingApprovalWire {
    pub fn from_agent(approval: coda_agent::PendingApproval) -> Self {
        let suggested_shell_allow_patterns = approval
            .calls
            .iter()
            .filter_map(|call| {
                suggested_shell_allow_pattern(call).map(|pattern| (call.id.clone(), pattern))
            })
            .collect();
        Self {
            thread_id: approval.thread_id,
            agent_name: approval.agent_name,
            parent_message_id: approval.parent_message_id,
            calls: approval.calls,
            suspended_at: approval.suspended_at,
            suggested_shell_allow_patterns,
        }
    }
}

fn suggested_shell_allow_pattern(call: &ToolCall) -> Option<String> {
    if call.name != "shell" {
        return None;
    }
    let command = extract_shell_command(call);
    ToolApprovalConfig::derive_shell_allow_pattern(&command)
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
