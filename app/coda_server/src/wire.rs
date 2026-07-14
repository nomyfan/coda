use crate::config::{ToolApprovalConfig, extract_shell_command};
use coda_agent::{AbortedTarget, AgentEvent, EventOrigin, ResumeDecision, SessionEvent};
use coda_core::llm::{AssistantMessage, Message, Modality, ReasoningEffort, ToolCall, ToolMessage};
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenSessionParams {
    pub workspace_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
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

/// `add_allow_pattern` params — append a glob to the shell allow-list; takes
/// effect immediately for the live session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddAllowPatternParams {
    pub workspace_id: String,
    pub pattern: String,
}

/// `set_model` params — switch the provider/model and reasoning setting. Applies
/// from the next turn (the server reopens the session). For reasoning models,
/// `null` selects the first configured effort, `none` turns thinking off, and any
/// configured level turns it on at that level. Models without reasoning controls
/// keep `null`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetModelParams {
    pub workspace_id: String,
    pub session_id: String,
    pub provider_id: String,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
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

/// Result of `set_model`: the selection now in effect (echoed on a real switch
/// and on an idempotent no-op).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSelection {
    pub provider_id: String,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Result of `open_session`, and the payload of an unsolicited `snapshot`
/// notification (hub re-attach): the resumed conversation history plus any
/// approvals left pending from a prior suspension, which the client must answer
/// with `resume` before the session resumes. `provider_id`/`reasoning_effort`
/// are the session's current model selection. `turn_running` tells the client a
/// turn is still in flight — its events are replayed (then streamed) right after.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub workspace_id: String,
    pub session_id: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub pending_approvals: Vec<PendingApprovalWire>,
    pub provider_id: String,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub turn_running: bool,
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
    pub reasoning_efforts: Vec<ReasoningEffort>,
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
    pub updated_at_ms: Option<u64>,
    #[serde(default)]
    pub first_user_message: Option<String>,
    #[serde(default)]
    pub has_pending_approval: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApprovalWire {
    pub thread_id: String,
    pub agent_name: String,
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
mod tests {
    use super::*;
    use coda_agent::{PendingApproval, ToolCallResolution};

    #[test]
    fn task_params_omits_empty_images() {
        let json = serde_json::to_string(&TaskParams {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            task: "hello".into(),
            images: vec![],
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"workspace_id":"coda","session_id":"s1","task":"hello"}"#
        );
    }

    #[test]
    fn open_session_params_defaults_takeover_off() {
        // Clients omit `takeover`; it defaults off (no silent eviction).
        let params: OpenSessionParams =
            serde_json::from_str(r#"{"workspace_id":"coda","session_id":"s1"}"#).unwrap();
        assert!(!params.takeover);
        assert!(params.provider_id.is_none());
        assert!(params.reasoning_effort.is_none());
    }

    #[test]
    fn resume_params_roundtrips() {
        let params = ResumeParams {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            agent_name: "coda".into(),
            thread_id: "t1".into(),
            decision: ResumeDecision {
                resolutions: vec![("call_1".into(), ToolCallResolution::Execute)],
            },
        };
        let back: ResumeParams =
            serde_json::from_str(&serde_json::to_string(&params).unwrap()).unwrap();
        assert_eq!(back.agent_name, "coda");
        assert_eq!(back.thread_id, "t1");
        assert_eq!(back.decision.resolutions.len(), 1);
    }

    #[test]
    fn session_ref_roundtrips() {
        let json = serde_json::to_string(&SessionRef {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"workspace_id":"coda","session_id":"s1"}"#);
    }

    #[test]
    fn add_allow_pattern_params_roundtrips() {
        let params: AddAllowPatternParams =
            serde_json::from_str(r#"{"workspace_id":"coda","pattern":"git *"}"#).unwrap();
        assert_eq!(params.workspace_id, "coda");
        assert_eq!(params.pattern, "git *");
    }

    #[test]
    fn set_model_params_defaults_effort_to_none() {
        let params: SetModelParams = serde_json::from_str(
            r#"{"workspace_id":"coda","session_id":"s1","provider_id":"deepseek"}"#,
        )
        .unwrap();
        assert_eq!(params.provider_id, "deepseek");
        assert!(params.reasoning_effort.is_none());
    }

    #[test]
    fn snapshot_serializes_without_type_tag() {
        let msg = Snapshot {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            messages: vec![],
            pending_approvals: vec![],
            provider_id: "deepseek".into(),
            reasoning_effort: Some(ReasoningEffort::High),
            turn_running: true,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"workspace_id":"coda","session_id":"s1","messages":[],"pending_approvals":[],"provider_id":"deepseek","reasoning_effort":"high","turn_running":true}"#
        );
    }

    #[test]
    fn snapshot_without_turn_running_defaults_to_false() {
        let json = r#"{"workspace_id":"coda","session_id":"s1","messages":[],"pending_approvals":[],"provider_id":"deepseek","reasoning_effort":null}"#;
        let snapshot: Snapshot = serde_json::from_str(json).unwrap();
        assert!(!snapshot.turn_running);
    }

    #[test]
    fn model_selection_roundtrips() {
        let result = ModelSelection {
            provider_id: "openai:gpt-4o".into(),
            reasoning_effort: None,
        };
        let back: ModelSelection =
            serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
        assert_eq!(back.provider_id, "openai:gpt-4o");
        assert!(back.reasoning_effort.is_none());
    }

    #[test]
    fn pending_approval_wire_suggests_shell_allow_patterns() {
        let approval = PendingApproval {
            thread_id: "t1".into(),
            agent_name: "coda".into(),
            calls: vec![
                ToolCall {
                    id: "call_shell".into(),
                    name: "shell".into(),
                    arguments: Some(r##"{"command":"# Run tests\ncargo test"}"##.into()),
                },
                ToolCall {
                    id: "call_read".into(),
                    name: "read_file".into(),
                    arguments: Some(r#"{"path":"README.md"}"#.into()),
                },
            ],
            suspended_at: jiff::Timestamp::default(),
        };

        let wire = PendingApprovalWire::from_agent(approval);

        assert_eq!(
            wire.suggested_shell_allow_patterns.get("call_shell"),
            Some(&"cargo test".to_string())
        );
        assert!(
            !wire
                .suggested_shell_allow_patterns
                .contains_key("call_read")
        );
    }

    #[test]
    fn pending_approval_wire_skips_compound_shell_calls() {
        let approval = PendingApproval {
            thread_id: "t1".into(),
            agent_name: "coda".into(),
            calls: vec![ToolCall {
                id: "call_shell".into(),
                name: "shell".into(),
                arguments: Some(
                    r##"{"command":"# Navigate\ncd /work/coda && cargo test"}"##.into(),
                ),
            }],
            suspended_at: jiff::Timestamp::default(),
        };

        let wire = PendingApprovalWire::from_agent(approval);

        assert!(wire.suggested_shell_allow_patterns.is_empty());
    }

    #[test]
    fn pending_approval_wire_skips_shell_calls_with_only_comments() {
        let approval = PendingApproval {
            thread_id: "t1".into(),
            agent_name: "coda".into(),
            calls: vec![ToolCall {
                id: "call_shell".into(),
                name: "shell".into(),
                arguments: Some(r##"{"command":"# just a comment"}"##.into()),
            }],
            suspended_at: jiff::Timestamp::default(),
        };

        let wire = PendingApprovalWire::from_agent(approval);

        assert!(wire.suggested_shell_allow_patterns.is_empty());
    }

    #[test]
    fn pending_approval_wire_skips_unresolvable_shell_calls() {
        let approval = PendingApproval {
            thread_id: "t1".into(),
            agent_name: "coda".into(),
            calls: vec![ToolCall {
                id: "call_shell".into(),
                name: "shell".into(),
                arguments: Some(r##"{"command":"git status > /tmp/out"}"##.into()),
            }],
            suspended_at: jiff::Timestamp::default(),
        };

        let wire = PendingApprovalWire::from_agent(approval);

        assert!(wire.suggested_shell_allow_patterns.is_empty());
    }

    #[test]
    fn set_model_params_roundtrips() {
        let params = SetModelParams {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            provider_id: "deepseek".into(),
            reasoning_effort: None,
        };
        let back: SetModelParams =
            serde_json::from_str(&serde_json::to_string(&params).unwrap()).unwrap();
        assert_eq!(back.provider_id, "deepseek");
        assert!(back.reasoning_effort.is_none());
    }

    #[test]
    fn workspace_catalog_roundtrips() {
        let msg = WorkspaceCatalog {
            workspaces: vec![WorkspaceSummaryWire {
                id: "coda".into(),
                path: "/work/coda".into(),
                sessions: vec![SessionSummaryWire {
                    id: "s1".into(),
                    updated_at_ms: Some(42),
                    first_user_message: Some("inspect the repo".into()),
                    has_pending_approval: true,
                }],
            }],
        };
        let back: WorkspaceCatalog =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(back.workspaces[0].id, "coda");
        assert_eq!(back.workspaces[0].sessions[0].id, "s1");
        assert!(back.workspaces[0].sessions[0].has_pending_approval);
    }

    #[test]
    fn provider_catalog_roundtrips() {
        let msg = ProviderCatalog {
            providers: vec![ProviderInfoWire {
                id: "deepseek:deepseek-reasoner".into(),
                provider: "deepseek".into(),
                model: "deepseek-reasoner".into(),
                context_window: 128_000,
                reasoning_efforts: vec![ReasoningEffort::Low, ReasoningEffort::High],
                input_modalities: vec![Modality::Text, Modality::Image],
            }],
            default_provider: "deepseek:deepseek-reasoner".into(),
        };
        let back: ProviderCatalog =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(back.providers[0].id, "deepseek:deepseek-reasoner");
        assert_eq!(back.providers[0].provider, "deepseek");
        assert_eq!(back.providers[0].context_window, 128_000);
        assert_eq!(back.providers[0].reasoning_efforts.len(), 2);
        assert_eq!(back.default_provider, "deepseek:deepseek-reasoner");
    }

    #[test]
    fn event_params_roundtrips() {
        let msg = EventParams {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            event: WireEvent::LlmContentChunk {
                agent_name: "coda".into(),
                thread_id: "t1".into(),
                content: "hi".into(),
            },
        };
        let back: EventParams =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(back.workspace_id, "coda");
        assert_eq!(back.session_id, "s1");
        assert!(matches!(
            back.event,
            WireEvent::LlmContentChunk { content, .. } if content == "hi"
        ));
    }
}
