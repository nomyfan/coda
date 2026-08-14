//! Serialization-layer types for checkpoints and runtime snapshots.
//!
//! These `Stored*` types carry `Serialize`/`Deserialize` and define the on-disk
//! format. Internal runtime types (`ResumePoint`, `AgentRuntimeSnapshot`, etc.)
//! are free to evolve independently; conversion happens at the load/save
//! boundary via `From` impls.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use coda_core::llm::{MessageId, ToolCall, ToolCallOutcome};

use crate::agent::{
    Envelope, HistoryEntry, PendingReply, PendingToolCall, ReplyTarget, ResumePoint,
    ToolExecutionState,
};
use crate::runtime::AgentRuntimeSnapshot;

// ---------------------------------------------------------------------------
// StoredCheckpoint
// ---------------------------------------------------------------------------

/// On-disk representation of a single agent thread's state.
///
/// Everything here is one of three things: the thread's conversation, tool
/// state anchored to it, or something that only makes sense *now* — where a
/// suspended run picks up, who is owed a reply.
///
/// Nothing here is a bare current value, and nothing new should be. A fork and
/// a rewind are both defined over turns: they choose a set of messages and keep
/// or drop them. A field that carries its newest value regardless is a field
/// those two will hand to a history that no longer explains it. Anything a
/// thread accumulates therefore goes in `state`, anchored to the message that
/// produced it, where the same cut reaches it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCheckpoint {
    pub thread_id: String,
    pub agent_name: String,
    /// The thread that spawned this one, and the name its `thread_id` was
    /// derived from (`uuid5(parent_thread_id, derivation_key)`). Both `None` on
    /// the root thread, so "no parent" is what identifies the root.
    ///
    /// The derivation is one-way, so without recording these the parent/child
    /// structure can only be re-guessed; a fork, which has to rebuild every
    /// derived id under a new root, needs to walk it directly. Kept separate
    /// from `reply_target`, which names the same parent but only for the span of
    /// one call and is cleared as soon as the reply is sent.
    #[serde(default)]
    pub parent_thread_id: Option<String>,
    #[serde(default)]
    pub derivation_key: Option<String>,
    #[serde(default)]
    pub reply_target: Option<ReplyTarget>,
    pub messages: Vec<HistoryEntry>,
    /// Tool state written by this thread, each entry anchored to the message
    /// that records the call that wrote it, in the order the messages appear.
    ///
    /// Anchored rather than kept as a current value, because that is what makes
    /// it cut with the conversation: the anchor is a `message_id`, which a fork
    /// copies verbatim and a rewind deletes along with its message. Reduce with
    /// last-wins per `kind` to get the value the thread holds now.
    #[serde(default)]
    pub state: Vec<StateEntry>,
    pub resume_point: StoredResumePoint,
    #[serde(default)]
    pub suspended_at: jiff::Timestamp,
}

/// One recorded value of one `kind`, and the message it hangs off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateEntry {
    pub message_id: MessageId,
    pub kind: String,
    pub value: serde_json::Value,
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
        parent_message_id: MessageId,
        pending_approval_calls: Vec<ToolCall>,
        pending_calls: Vec<StoredPendingToolCall>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToolExecutionState {
    /// The assistant message this batch of calls came from, so a sub-agent
    /// dispatched after a restart can still record what triggered it.
    pub parent_message_id: MessageId,
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
            parent_message_id: s.parent_message_id,
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
                parent_message_id,
                pending_approval_calls,
                pending_calls,
            } => StoredResumePoint::PendingApproval {
                parent_message_id,
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
            parent_message_id: s.parent_message_id,
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
                parent_message_id,
                pending_approval_calls,
                pending_calls,
            } => ResumePoint::PendingApproval {
                parent_message_id,
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
