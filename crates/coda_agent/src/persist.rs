//! Serialization-layer types for checkpoints and runtime snapshots.
//!
//! These `Stored*` types carry `Serialize`/`Deserialize` and define the on-disk
//! format. Internal runtime types (`ResumePoint`, `AgentRuntimeSnapshot`, etc.)
//! are free to evolve independently; conversion happens at the load/save
//! boundary via `From` impls.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use coda_core::llm::{Message, ToolCall, ToolCallOutcome};
use coda_tools::TodoItem;

use crate::agent::{
    Envelope, PendingReply, PendingToolCall, ReplyTarget, ResumePoint, ToolExecutionState,
};
use crate::runtime::AgentRuntimeSnapshot;

// ---------------------------------------------------------------------------
// StoredCheckpoint
// ---------------------------------------------------------------------------

/// On-disk representation of a single agent thread's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCheckpoint {
    pub thread_id: String,
    pub agent_name: String,
    #[serde(default)]
    pub reply_target: Option<ReplyTarget>,
    pub messages: Vec<Message>,
    pub todos: Vec<TodoItem>,
    pub resume_point: StoredResumePoint,
    #[serde(default)]
    pub suspended_at: jiff::Timestamp,
}

// ---------------------------------------------------------------------------
// StoredResumePoint
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum StoredResumePoint {
    #[default]
    Generation,
    ToolExecution(StoredToolExecutionState),
    PendingApproval {
        pending_approval_calls: Vec<ToolCall>,
        pending_calls: Vec<StoredPendingToolCall>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToolExecutionState {
    pub pending_replies: Vec<PendingReply>,
    pub tool_calls: Vec<StoredPendingToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPendingToolCall {
    pub tool_call: ToolCall,
    pub outcome: ToolCallOutcome,
}

// ---------------------------------------------------------------------------
// StoredRuntimeSnapshot
// ---------------------------------------------------------------------------

/// On-disk representation of the per-session runtime state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRuntimeSnapshot {
    pub drained_envelopes: HashMap<String, Vec<Envelope>>,
    pub agent_drained_envelopes: HashMap<String, Vec<Envelope>>,
    pub active_threads: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// From impls: internal → stored
// ---------------------------------------------------------------------------

impl From<PendingToolCall> for StoredPendingToolCall {
    fn from(p: PendingToolCall) -> Self {
        StoredPendingToolCall {
            tool_call: p.tool_call,
            outcome: p.outcome,
        }
    }
}

impl From<ToolExecutionState> for StoredToolExecutionState {
    fn from(s: ToolExecutionState) -> Self {
        StoredToolExecutionState {
            pending_replies: s.pending_replies,
            tool_calls: s.tool_calls.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<ResumePoint> for StoredResumePoint {
    fn from(rp: ResumePoint) -> Self {
        match rp {
            ResumePoint::Generation => StoredResumePoint::Generation,
            ResumePoint::ToolExecution(state) => StoredResumePoint::ToolExecution(state.into()),
            ResumePoint::PendingApproval {
                pending_approval_calls,
                pending_calls,
            } => StoredResumePoint::PendingApproval {
                pending_approval_calls: pending_approval_calls.into(),
                pending_calls: pending_calls.into_iter().map(Into::into).collect(),
            },
        }
    }
}

impl From<AgentRuntimeSnapshot> for StoredRuntimeSnapshot {
    fn from(s: AgentRuntimeSnapshot) -> Self {
        StoredRuntimeSnapshot {
            drained_envelopes: s.drained_envelopes,
            agent_drained_envelopes: s.agent_drained_envelopes,
            active_threads: s.active_threads,
        }
    }
}

// ---------------------------------------------------------------------------
// From impls: stored → internal
// ---------------------------------------------------------------------------

impl From<StoredPendingToolCall> for PendingToolCall {
    fn from(p: StoredPendingToolCall) -> Self {
        PendingToolCall {
            tool_call: p.tool_call,
            outcome: p.outcome,
        }
    }
}

impl From<StoredToolExecutionState> for ToolExecutionState {
    fn from(s: StoredToolExecutionState) -> Self {
        ToolExecutionState {
            pending_replies: s.pending_replies,
            tool_calls: s.tool_calls.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<StoredResumePoint> for ResumePoint {
    fn from(rp: StoredResumePoint) -> Self {
        match rp {
            StoredResumePoint::Generation => ResumePoint::Generation,
            StoredResumePoint::ToolExecution(state) => ResumePoint::ToolExecution(state.into()),
            StoredResumePoint::PendingApproval {
                pending_approval_calls,
                pending_calls,
            } => ResumePoint::PendingApproval {
                pending_approval_calls: pending_approval_calls.into(),
                pending_calls: pending_calls.into_iter().map(Into::into).collect(),
            },
        }
    }
}

impl From<StoredRuntimeSnapshot> for AgentRuntimeSnapshot {
    fn from(s: StoredRuntimeSnapshot) -> Self {
        AgentRuntimeSnapshot {
            drained_envelopes: s.drained_envelopes,
            agent_drained_envelopes: s.agent_drained_envelopes,
            active_threads: s.active_threads,
        }
    }
}
