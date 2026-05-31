use coda_agent::{
    AbortedTarget, AgentEvent, EventOrigin, PendingApproval, ResumeDecision, SessionEvent,
};
use coda_core::llm::{AssistantMessage, Message, ToolCall, ToolMessage};
use serde::{Deserialize, Serialize};

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
        approval: PendingApproval,
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
                approval,
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
    /// Start a new turn with a user task.
    Task { task: String },
    /// Answer a suspended tool call. `agent_name` and `thread_id` come from the
    /// [`PendingApproval`] carried by a [`ServerMessage::Event`] `Suspended`.
    Resume {
        agent_name: String,
        thread_id: String,
        decision: ResumeDecision,
    },
    /// Interrupt whatever the session is currently doing.
    Abort,
    /// Append a glob pattern to the shell allow-list. Takes effect immediately
    /// for the live session.
    AddAllowPattern { pattern: String },
}

/// A message pushed by the server over the WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Sent once, immediately after connect: the resumed conversation history
    /// plus any approvals left pending from a prior suspension, which the client
    /// must answer with `Resume` before the session resumes.
    Snapshot {
        session_id: String,
        messages: Vec<Message>,
        #[serde(default)]
        pending_approvals: Vec<PendingApproval>,
    },
    /// A live runtime event. Nested under `event` rather than flattened so the
    /// inner `type` tag of [`WireEvent`] does not collide with this enum's tag.
    Event { event: WireEvent },
    /// Result of a requested shell allow-list update.
    AllowPatternResult {
        pattern: String,
        #[serde(default)]
        error: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_agent::ToolCallResolution;

    fn roundtrip_client(msg: &ClientMessage) -> ClientMessage {
        serde_json::from_str(&serde_json::to_string(msg).unwrap()).unwrap()
    }

    #[test]
    fn client_task_roundtrips() {
        let json = serde_json::to_string(&ClientMessage::Task {
            task: "hello".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"task","task":"hello"}"#);
    }

    #[test]
    fn client_abort_roundtrips() {
        let json = serde_json::to_string(&ClientMessage::Abort).unwrap();
        assert_eq!(json, r#"{"type":"abort"}"#);
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(&json).unwrap(),
            ClientMessage::Abort
        ));
    }

    #[test]
    fn client_resume_roundtrips() {
        let msg = ClientMessage::Resume {
            agent_name: "coda".into(),
            thread_id: "t1".into(),
            decision: ResumeDecision {
                resolutions: vec![("call_1".into(), ToolCallResolution::Execute)],
            },
        };
        match roundtrip_client(&msg) {
            ClientMessage::Resume {
                agent_name,
                thread_id,
                decision,
            } => {
                assert_eq!(agent_name, "coda");
                assert_eq!(thread_id, "t1");
                assert_eq!(decision.resolutions.len(), 1);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn client_add_allow_pattern_roundtrips() {
        let msg = ClientMessage::AddAllowPattern {
            pattern: "git *".into(),
        };
        assert!(matches!(
            roundtrip_client(&msg),
            ClientMessage::AddAllowPattern { pattern } if pattern == "git *"
        ));
    }

    #[test]
    fn server_snapshot_roundtrips() {
        let msg = ServerMessage::Snapshot {
            session_id: "s1".into(),
            messages: vec![],
            pending_approvals: vec![],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"snapshot","session_id":"s1","messages":[],"pending_approvals":[]}"#
        );
    }

    #[test]
    fn server_event_roundtrips() {
        let msg = ServerMessage::Event {
            event: WireEvent::LlmContentChunk {
                agent_name: "coda".into(),
                thread_id: "t1".into(),
                content: "hi".into(),
            },
        };
        match serde_json::from_str::<ServerMessage>(&serde_json::to_string(&msg).unwrap()).unwrap()
        {
            ServerMessage::Event {
                event: WireEvent::LlmContentChunk { content, .. },
            } => {
                assert_eq!(content, "hi");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn server_allow_pattern_result_roundtrips() {
        let ok = ServerMessage::AllowPatternResult {
            pattern: "git *".into(),
            error: None,
        };
        let json = serde_json::to_string(&ok).unwrap();
        assert_eq!(
            json,
            r#"{"type":"allow_pattern_result","pattern":"git *","error":null}"#
        );

        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::AllowPatternResult { pattern, error } => {
                assert_eq!(pattern, "git *");
                assert!(error.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
