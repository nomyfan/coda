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

/// A command sent by the client over the WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Request the configured workspace/session catalog.
    ListWorkspaces,
    /// Request the selectable providers (static for the server's lifetime).
    ListProviders,
    /// Open or switch the active session on this connection. `provider_id` and
    /// `reasoning_effort` carry a client-chosen selection (e.g. picked on a new
    /// session before the first message); both default to the server's defaults
    /// when omitted.
    OpenSession {
        workspace_id: String,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<ReasoningEffort>,
    },
    /// Start a new turn with a user task, optionally with image attachments.
    Task {
        workspace_id: String,
        session_id: String,
        task: String,
        /// Base64 data-URIs (`data:image/<fmt>;base64,<b64>`) or HTTPS URLs.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    /// Answer a suspended tool call. `agent_name` and `thread_id` come from the
    /// [`PendingApprovalWire`] carried by a [`ServerMessage::Event`] `Suspended`.
    Resume {
        workspace_id: String,
        session_id: String,
        agent_name: String,
        thread_id: String,
        decision: ResumeDecision,
    },
    /// Interrupt whatever the session is currently doing.
    Abort {
        workspace_id: String,
        session_id: String,
    },
    /// Delete a session: stop it if live, then remove its persisted directory.
    DeleteSession {
        workspace_id: String,
        session_id: String,
    },
    /// Close a live session: free its runtime memory while keeping the persisted
    /// session on disk so it can be reopened later. An idle session is torn down
    /// at once; one with a turn in flight is torn down when that turn settles
    /// (finishes, suspends, aborts, or errors), so running work isn't aborted.
    /// Reopening the session before then cancels the close. A no-op when the
    /// session isn't currently live.
    CloseSession {
        workspace_id: String,
        session_id: String,
    },
    /// Append a glob pattern to the shell allow-list. Takes effect immediately
    /// for the live session.
    AddAllowPattern {
        workspace_id: String,
        pattern: String,
    },
    /// Switch the provider/model and reasoning setting a session uses. Applies
    /// from the next turn (the server reopens the session). For reasoning models,
    /// `null` selects the first configured effort, `none` turns thinking off, and
    /// any configured level turns it on at that level. Models without reasoning
    /// controls keep `null`.
    SetModel {
        workspace_id: String,
        session_id: String,
        provider_id: String,
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
    },
}

/// A message pushed by the server over the WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Configured workspaces and persisted sessions.
    WorkspaceCatalog {
        workspaces: Vec<WorkspaceSummaryWire>,
    },
    /// The providers the dashboard can choose between and the one new sessions
    /// default to. Static for the server's lifetime; fetched once per connection.
    ProviderCatalog {
        providers: Vec<ProviderInfoWire>,
        default_provider: String,
    },
    /// Confirms a successful provider/reasoning switch for a live session.
    ModelChanged {
        workspace_id: String,
        session_id: String,
        provider_id: String,
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
    },
    /// Sent once, immediately after connect: the resumed conversation history
    /// plus any approvals left pending from a prior suspension, which the client
    /// must answer with `Resume` before the session resumes. `provider_id` and
    /// `reasoning_effort` are the session's current model selection.
    Snapshot {
        workspace_id: String,
        session_id: String,
        messages: Vec<Message>,
        #[serde(default)]
        pending_approvals: Vec<PendingApprovalWire>,
        provider_id: String,
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
    },
    /// A live runtime event. Nested under `event` rather than flattened so the
    /// inner `type` tag of [`WireEvent`] does not collide with this enum's tag.
    Event {
        workspace_id: String,
        session_id: String,
        event: WireEvent,
    },
    /// Result of a requested shell allow-list update.
    AllowPatternResult {
        workspace_id: String,
        pattern: String,
        #[serde(default)]
        error: Option<String>,
    },
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

    fn roundtrip_client(msg: &ClientMessage) -> ClientMessage {
        serde_json::from_str(&serde_json::to_string(msg).unwrap()).unwrap()
    }

    #[test]
    fn client_task_roundtrips() {
        let json = serde_json::to_string(&ClientMessage::Task {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            task: "hello".into(),
            images: vec![],
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"task","workspace_id":"coda","session_id":"s1","task":"hello"}"#
        );
    }

    #[test]
    fn client_list_workspaces_roundtrips() {
        let json = serde_json::to_string(&ClientMessage::ListWorkspaces).unwrap();
        assert_eq!(json, r#"{"type":"list_workspaces"}"#);
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(&json).unwrap(),
            ClientMessage::ListWorkspaces
        ));
    }

    #[test]
    fn client_open_session_roundtrips() {
        let json = serde_json::to_string(&ClientMessage::OpenSession {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            provider_id: None,
            reasoning_effort: None,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"open_session","workspace_id":"coda","session_id":"s1"}"#
        );
    }

    #[test]
    fn client_abort_roundtrips() {
        let json = serde_json::to_string(&ClientMessage::Abort {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"abort","workspace_id":"coda","session_id":"s1"}"#
        );
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(&json).unwrap(),
            ClientMessage::Abort { .. }
        ));
    }

    #[test]
    fn client_resume_roundtrips() {
        let msg = ClientMessage::Resume {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            agent_name: "coda".into(),
            thread_id: "t1".into(),
            decision: ResumeDecision {
                resolutions: vec![("call_1".into(), ToolCallResolution::Execute)],
            },
        };
        match roundtrip_client(&msg) {
            ClientMessage::Resume {
                workspace_id,
                session_id,
                agent_name,
                thread_id,
                decision,
            } => {
                assert_eq!(workspace_id, "coda");
                assert_eq!(session_id, "s1");
                assert_eq!(agent_name, "coda");
                assert_eq!(thread_id, "t1");
                assert_eq!(decision.resolutions.len(), 1);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn client_close_session_roundtrips() {
        let json = serde_json::to_string(&ClientMessage::CloseSession {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"close_session","workspace_id":"coda","session_id":"s1"}"#
        );
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(&json).unwrap(),
            ClientMessage::CloseSession { .. }
        ));
    }

    #[test]
    fn client_add_allow_pattern_roundtrips() {
        let msg = ClientMessage::AddAllowPattern {
            workspace_id: "coda".into(),
            pattern: "git *".into(),
        };
        assert!(matches!(
            roundtrip_client(&msg),
            ClientMessage::AddAllowPattern { workspace_id, pattern } if workspace_id == "coda" && pattern == "git *"
        ));
    }

    #[test]
    fn server_snapshot_roundtrips() {
        let msg = ServerMessage::Snapshot {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            messages: vec![],
            pending_approvals: vec![],
            provider_id: "deepseek".into(),
            reasoning_effort: Some(ReasoningEffort::High),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"snapshot","workspace_id":"coda","session_id":"s1","messages":[],"pending_approvals":[],"provider_id":"deepseek","reasoning_effort":"high"}"#
        );
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
    fn client_set_model_roundtrips() {
        let msg = ClientMessage::SetModel {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            provider_id: "deepseek".into(),
            reasoning_effort: None,
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"set_model","workspace_id":"coda","session_id":"s1","provider_id":"deepseek","reasoning_effort":null}"#
        );
        assert!(matches!(
            roundtrip_client(&msg),
            ClientMessage::SetModel { provider_id, reasoning_effort, .. }
                if provider_id == "deepseek" && reasoning_effort.is_none()
        ));
    }

    #[test]
    fn server_workspace_catalog_roundtrips() {
        let msg = ServerMessage::WorkspaceCatalog {
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
        match serde_json::from_str::<ServerMessage>(&serde_json::to_string(&msg).unwrap()).unwrap()
        {
            ServerMessage::WorkspaceCatalog { workspaces } => {
                assert_eq!(workspaces[0].id, "coda");
                assert_eq!(workspaces[0].sessions[0].id, "s1");
                assert!(workspaces[0].sessions[0].has_pending_approval);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn server_provider_catalog_roundtrips() {
        let msg = ServerMessage::ProviderCatalog {
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
        match serde_json::from_str::<ServerMessage>(&serde_json::to_string(&msg).unwrap()).unwrap()
        {
            ServerMessage::ProviderCatalog {
                providers,
                default_provider,
            } => {
                assert_eq!(providers[0].id, "deepseek:deepseek-reasoner");
                assert_eq!(providers[0].provider, "deepseek");
                assert_eq!(providers[0].context_window, 128_000);
                assert_eq!(providers[0].reasoning_efforts.len(), 2);
                assert_eq!(default_provider, "deepseek:deepseek-reasoner");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn server_model_changed_roundtrips() {
        let msg = ServerMessage::ModelChanged {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            provider_id: "openai:gpt-4o".into(),
            reasoning_effort: None,
        };
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(&serde_json::to_string(&msg).unwrap()).unwrap(),
            ServerMessage::ModelChanged { provider_id, reasoning_effort, .. }
                if provider_id == "openai:gpt-4o" && reasoning_effort.is_none()
        ));
    }

    #[test]
    fn server_event_roundtrips() {
        let msg = ServerMessage::Event {
            workspace_id: "coda".into(),
            session_id: "s1".into(),
            event: WireEvent::LlmContentChunk {
                agent_name: "coda".into(),
                thread_id: "t1".into(),
                content: "hi".into(),
            },
        };
        match serde_json::from_str::<ServerMessage>(&serde_json::to_string(&msg).unwrap()).unwrap()
        {
            ServerMessage::Event {
                workspace_id,
                session_id,
                event: WireEvent::LlmContentChunk { content, .. },
            } => {
                assert_eq!(workspace_id, "coda");
                assert_eq!(session_id, "s1");
                assert_eq!(content, "hi");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn server_allow_pattern_result_roundtrips() {
        let ok = ServerMessage::AllowPatternResult {
            workspace_id: "coda".into(),
            pattern: "git *".into(),
            error: None,
        };
        let json = serde_json::to_string(&ok).unwrap();
        assert_eq!(
            json,
            r#"{"type":"allow_pattern_result","workspace_id":"coda","pattern":"git *","error":null}"#
        );

        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::AllowPatternResult {
                workspace_id,
                pattern,
                error,
            } => {
                assert_eq!(workspace_id, "coda");
                assert_eq!(pattern, "git *");
                assert!(error.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
