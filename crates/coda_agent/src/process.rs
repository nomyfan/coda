//! A logical process has one durable identity and private working state.
use crate::{
    Program,
    agent::{HistoryEntry, ThreadStateMap},
    message_view,
};
use coda_core::llm::{CompletionUsage, Message, MessageId, RequestMessage, SystemMessage, TurnId};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(crate) struct ProcessOrigin {
    pub parent_thread_id: String,
    pub derivation_key: String,
}

pub struct Process {
    pub pid: ProcessId,
    pub program: Arc<Program>,
    pub(crate) state: Arc<Mutex<ProcessMemory>>,
    pub(crate) resume_point: crate::agent::ResumePoint,
    pub(crate) execution: Option<crate::execution::StoredExecution>,
    pub(crate) reply_target: Option<crate::agent::ReplyTarget>,
    pub(crate) origin: Option<ProcessOrigin>,
}

/// A thread's history and what the runtime derives from it.
///
/// Every field is private: `state` only stays true to `messages` because
/// [`restore`](Self::restore) and [`record`](Self::record) are the sole way to
/// change either, and a caller that could reach past them would silently make
/// [`Process::state_snapshot`] wrong. Reads go through `Process`, which already
/// exposes each one that has a caller.
#[derive(Default)]
pub(crate) struct ProcessMemory {
    messages: Vec<HistoryEntry>,
    /// Every entry's `state` reduced last-wins. Derived, never persisted: a tool
    /// batch asks for it before each of its calls, and rebuilding it from a
    /// transcript that only ever grows would cost more every turn.
    state: ThreadStateMap,
    /// The turn newly appended messages belong to. Advances only when a user
    /// message is appended (see [`Process::add_opening_message`]); `None` only before
    /// this thread has any history at all.
    current_turn: Option<TurnId>,
}

#[derive(Eq, Hash, PartialEq, Clone, Debug, Serialize, Deserialize)]
pub struct ProcessId(pub(crate) String);

impl Default for ProcessId {
    fn default() -> Self {
        Self::new()
    }
}

/// Namespace for hashing a non-UUID thread id into a usable uuid5 namespace.
/// Arbitrary but fixed: changing it changes every derived thread id.
const NON_UUID_THREAD_NAMESPACE: Uuid = Uuid::from_u128(0x3f7a1c62_5be4_4d0f_9a31_c6d84b7e02f5);

impl ProcessId {
    pub fn new() -> Self {
        ProcessId(Uuid::new_v4().to_string())
    }

    /// Derive a child thread id from its parent and a name.
    ///
    /// A parent id that isn't a UUID — the root thread id is the client-supplied
    /// session id, which is only required to be a safe string — is hashed into a
    /// namespace rather than falling back to the nil one. Falling back would
    /// give every such session the *same* namespace, so two sessions would
    /// derive identical child ids and "a different parent means different
    /// children" would silently stop holding.
    pub fn from_uuid5(namespace: &ProcessId, name: &str) -> Self {
        let ns = Uuid::parse_str(&namespace.0)
            .unwrap_or_else(|_| Uuid::new_v5(&NON_UUID_THREAD_NAMESPACE, namespace.0.as_bytes()));
        ProcessId(Uuid::new_v5(&ns, name.as_bytes()).to_string())
    }
}

impl AsRef<str> for ProcessId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for ProcessId {
    fn from(s: String) -> Self {
        ProcessId(s)
    }
}

impl Process {
    pub(crate) fn new(pid: ProcessId, program: Arc<Program>) -> Self {
        Self {
            pid,
            program,
            state: Arc::new(Mutex::new(ProcessMemory::default())),
            resume_point: Default::default(),
            execution: None,
            reply_target: None,
            origin: None,
        }
    }

    pub fn name(&self) -> &str {
        &self.program.name
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
    /// Record the message that opens a turn: what the user sent, or the notice
    /// the runtime wrote when background work finished. Both start a turn; only
    /// these two kinds ever do.
    pub async fn add_opening_message(&self, turn_id: TurnId, message: Message) {
        debug!("Adding turn-opening message: {:?}", message);
        let mut state = self.state.lock().await;
        state.current_turn = Some(turn_id);
        let persisted_notice = matches!(&message, Message::TaskNotice(notice) if state.messages.iter().any(|entry| matches!(&entry.message, Message::TaskNotice(existing) if existing.message_id == notice.message_id)));
        if !persisted_notice {
            state.messages.push(HistoryEntry::new(turn_id, message));
        }
    }

    /// Append a message to the current turn. Used for assistant and tool
    /// messages, which never start a turn.
    pub async fn add_message(&self, message: Message) {
        debug!("Adding message: {:?}", message);
        let mut state = self.state.lock().await;
        let turn_id = state.stamp();
        state.messages.push(HistoryEntry::new(turn_id, message));
    }

    pub async fn add_messages(&self, messages: Vec<Message>) {
        debug!("Adding messages: {:?}", messages);
        let mut state = self.state.lock().await;
        let turn_id = state.stamp();
        state.messages.extend(
            messages
                .into_iter()
                .map(|message| HistoryEntry::new(turn_id, message)),
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
    /// [`ProcessMemory::stamp`], so merely *asking* on a fresh thread minted a
    /// throwaway turn and logged its invariant break.
    pub async fn current_turn(&self) -> Option<TurnId> {
        self.state.lock().await.current_turn
    }

    /// The request this thread's history makes: the system prompt, then the
    /// part of history a compaction left visible, lowered to what a provider
    /// accepts.
    pub async fn messages(&self) -> Result<Vec<RequestMessage>, message_view::InvalidHistory> {
        let history = self.state.lock().await;
        let visible: Vec<_> = message_view::model_view(&history.messages).collect();
        message_view::validate_messages(visible.iter().map(|entry| &entry.message))?;
        let mut messages = Vec::with_capacity(history.messages.len() + 1);
        messages.push(RequestMessage::System(SystemMessage(
            self.program.system_prompt.resolve(),
        )));
        messages.extend(
            visible
                .into_iter()
                .filter_map(|entry| (&entry.message).into()),
        );
        Ok(messages)
    }

    /// Returns conversation history without the system prompt (suitable for checkpointing).
    pub async fn history(&self) -> Vec<HistoryEntry> {
        self.state.lock().await.messages.clone()
    }

    /// The most recent recorded token usage on this thread, read without
    /// cloning the transcript.
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

    /// Restore this process's conversation from its checkpoint. The tool state each entry carries stays opaque here;
    /// tools interpret their own kinds through `ToolCallContext::state`.
    pub async fn restore_history(&self, messages: Vec<HistoryEntry>) {
        let mut state = self.state.lock().await;
        state.restore(messages);
        // Whatever work is being resumed belongs to the turn of the last message
        // written, so the turn needs no separate persistence.
        state.current_turn = state.messages.last().map(|entry| entry.turn_id);
    }

    /// This thread's recorded state so far, reduced to one value per kind.
    /// Last-wins, because every entry is a complete value rather than a delta.
    pub async fn state_snapshot(&self) -> ThreadStateMap {
        self.state.lock().await.state.clone()
    }

    /// Append a message together with whatever the call that produced it
    /// recorded, as one entry — so state can neither lose its message nor
    /// outlive it. `recorded` is in write order, so a kind written twice keeps
    /// the last value, which is the one the call established.
    pub async fn add_message_with_state(
        &self,
        message: Message,
        recorded: Vec<(String, serde_json::Value)>,
    ) {
        self.state
            .lock()
            .await
            .record(message, recorded.into_iter().collect());
    }
}

impl ProcessMemory {
    pub(crate) fn messages(&self) -> Vec<Message> {
        self.messages
            .iter()
            .map(|entry| entry.message.clone())
            .collect()
    }

    /// Rebuild derived state when restoring a checkpoint, including empty history.
    fn restore(&mut self, messages: Vec<HistoryEntry>) {
        self.state = messages
            .iter()
            .flat_map(|entry| entry.state.iter())
            .map(|(kind, value)| (kind.clone(), value.clone()))
            .collect();
        self.messages = messages;
    }

    /// Append a message and the state its call recorded, in step.
    fn record(&mut self, message: Message, state: ThreadStateMap) {
        let turn_id = self.stamp();
        self.state.extend(
            state
                .iter()
                .map(|(kind, value)| (kind.clone(), value.clone())),
        );
        self.messages.push(HistoryEntry {
            turn_id,
            message,
            state,
        });
    }

    /// The turn to tag a newly appended assistant/tool message with. Only the
    /// append paths call this — reading the turn goes through
    /// [`Process::current_turn`], which reports "no turn yet" rather than minting
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

#[cfg(test)]
mod thread_id_tests {
    use super::*;

    /// The root thread id is whatever session id the client chose, and it is not
    /// required to be a UUID — the web client falls back to a non-UUID form
    /// whenever `crypto.randomUUID` is unavailable, which is every plain-HTTP
    /// origin. Two such sessions must still derive distinct child threads.
    #[test]
    fn non_uuid_parents_derive_distinct_children() {
        let one = ProcessId::from("session-mf3k2x".to_string());
        let other = ProcessId::from("session-mf3k2y".to_string());

        assert_ne!(
            ProcessId::from_uuid5(&one, "explore"),
            ProcessId::from_uuid5(&other, "explore")
        );
    }

    /// Deriving from a parent that *is* a UUID must keep using it as the
    /// namespace directly, so existing stateful thread ids are unaffected by the
    /// non-UUID handling above.
    #[test]
    fn uuid_parent_is_used_as_the_namespace_directly() {
        let parent = ProcessId::from("6ba7b810-9dad-11d1-80b4-00c04fd430c8".to_string());

        assert_eq!(
            ProcessId::from_uuid5(&parent, "explore").as_ref(),
            Uuid::new_v5(&Uuid::parse_str(parent.as_ref()).unwrap(), b"explore").to_string()
        );
    }
}

#[cfg(test)]
mod thread_state_tests {
    use super::*;
    use coda_core::llm::{ToolCallOutcome, ToolMessage, ToolOutput};
    use serde_json::json;

    /// A tool result carrying one recorded value — the only shape that fills
    /// [`HistoryEntry::state`].
    fn recorded(turn: TurnId, kind: &str, value: &str) -> HistoryEntry {
        HistoryEntry {
            state: [(kind.to_string(), json!(value))].into_iter().collect(),
            ..HistoryEntry::new(turn, tool_message())
        }
    }

    fn tool_message() -> Message {
        Message::Tool(ToolMessage::new(
            "call".to_string(),
            "a_tool".to_string(),
            ToolOutput::Ok("done".to_string()),
            ToolCallOutcome::Auto,
            None,
        ))
    }

    #[test]
    fn a_restored_thread_reduces_its_snapshot_from_the_messages_it_was_handed() {
        let turn = TurnId::from(MessageId::new());
        let mut state = ProcessMemory::default();
        state.restore(vec![
            recorded(turn, "plan", "first"),
            recorded(turn, "plan", "second"),
            recorded(turn, "notes", "kept"),
        ]);
        assert_eq!(state.state.get("plan"), Some(&json!("second")));
        assert_eq!(state.state.get("notes"), Some(&json!("kept")));
    }

    /// Restoring empty history also clears any previously derived state.
    #[test]
    fn restoring_another_thread_leaves_none_of_the_last_ones_state() {
        let turn = TurnId::from(MessageId::new());
        let mut state = ProcessMemory::default();
        state.restore(vec![recorded(turn, "plan", "first")]);
        state.restore(vec![]);
        assert!(state.state.is_empty());
    }

    #[test]
    fn a_recorded_write_lands_on_the_message_and_in_the_snapshot() {
        let turn = TurnId::from(MessageId::new());
        let mut state = ProcessMemory {
            current_turn: Some(turn),
            ..Default::default()
        };
        state.record(
            tool_message(),
            [("plan".to_string(), json!("now"))].into_iter().collect(),
        );
        let entry = state.messages.last().expect("the message was appended");
        assert_eq!(entry.state.get("plan"), Some(&json!("now")));
        assert_eq!(state.state.get("plan"), Some(&json!("now")));
    }
}
