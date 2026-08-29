//! What a thread shows the model — the model view.
//!
//! [`model_view`] starts at the last compaction summary (if any), then
//! everything after its recorded `cutoff`, in original order. Storage only
//! ever grows at the tail, so a summary can land after messages it actually
//! protects (an in-progress turn); `cutoff` is what lets the view put them
//! back after the summary instead of before it. Transcript-only records (a
//! failed compaction) are filtered out.

use crate::agent::HistoryEntry;
use coda_core::llm::{Message, MessageId};
use std::collections::HashMap;

/// The model's view of `messages`: the last compaction summary (if any)
/// leading, then everything after its `cutoff`, minus transcript-only
/// records. A thread with no summary yet is shown whole.
pub fn model_view(messages: &[HistoryEntry]) -> impl Iterator<Item = &HistoryEntry> + '_ {
    let (summary, tail) = model_view_parts_indexed(messages);
    summary.into_iter().chain(tail).map(|(_, entry)| entry)
}

/// Splits the model view into the active summary and the uncompressed tail.
/// Both retain their physical index in `messages`, which compaction needs for
/// its persisted coverage watermark.
pub(crate) fn model_view_parts_indexed(
    messages: &[HistoryEntry],
) -> (
    Option<(usize, &HistoryEntry)>,
    impl Iterator<Item = (usize, &HistoryEntry)> + '_,
) {
    let boundary = last_summary(messages);
    let tail_start = boundary.map_or(0, |(_, tail_start)| tail_start);
    let summary = boundary.map(|(summary_idx, _)| (summary_idx, &messages[summary_idx]));
    let summary_id = summary.map(|(_, entry)| entry.message.message_id());
    let visible_summary = summary.filter(|(_, entry)| entry.message.visible_to_model());
    let tail = messages[tail_start..]
        .iter()
        .enumerate()
        .map(move |(offset, entry)| (tail_start + offset, entry))
        .filter(move |(_, entry)| Some(entry.message.message_id()) != summary_id)
        .filter(|(_, entry)| entry.message.visible_to_model());
    (visible_summary, tail)
}

/// A tool-call/result protocol violation in the messages visible to a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidHistory {
    OrphanToolResult {
        message_id: MessageId,
        call_id: String,
    },
    DuplicateToolCall {
        message_id: MessageId,
        call_id: String,
    },
    DuplicateToolResult {
        message_id: MessageId,
        call_id: String,
    },
    IncompleteToolBatch {
        assistant_message_id: MessageId,
        missing_call_ids: Vec<String>,
    },
}

impl std::fmt::Display for InvalidHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OrphanToolResult {
                message_id,
                call_id,
            } => write!(
                f,
                "tool result {message_id} references call '{call_id}' without a matching assistant batch"
            ),
            Self::DuplicateToolCall {
                message_id,
                call_id,
            } => write!(
                f,
                "assistant message {message_id} declares tool call '{call_id}' more than once"
            ),
            Self::DuplicateToolResult {
                message_id,
                call_id,
            } => write!(
                f,
                "tool result {message_id} answers call '{call_id}' more than once"
            ),
            Self::IncompleteToolBatch {
                assistant_message_id,
                missing_call_ids,
            } => write!(
                f,
                "assistant message {assistant_message_id} is missing results for calls: {}",
                missing_call_ids.join(", ")
            ),
        }
    }
}

impl std::error::Error for InvalidHistory {}

/// Validates the exact history sequence an ordinary provider request sees.
pub fn validate_model_view(messages: &[HistoryEntry]) -> Result<(), InvalidHistory> {
    validate_messages(model_view(messages).map(|entry| &entry.message))
}

pub(crate) fn validate_messages<'a>(
    messages: impl IntoIterator<Item = &'a Message>,
) -> Result<(), InvalidHistory> {
    struct Batch {
        assistant_message_id: MessageId,
        expected: HashMap<String, bool>,
    }

    fn finish_batch(batch: &mut Option<Batch>) -> Result<(), InvalidHistory> {
        let Some(open) = batch.take() else {
            return Ok(());
        };
        let mut missing_call_ids: Vec<_> = open
            .expected
            .into_iter()
            .filter_map(|(id, answered)| (!answered).then_some(id))
            .collect();
        if missing_call_ids.is_empty() {
            return Ok(());
        }
        missing_call_ids.sort();
        Err(InvalidHistory::IncompleteToolBatch {
            assistant_message_id: open.assistant_message_id,
            missing_call_ids,
        })
    }

    let mut batch: Option<Batch> = None;
    for message in messages {
        match message {
            Message::Tool(tool) => {
                let Some(open) = &mut batch else {
                    return Err(InvalidHistory::OrphanToolResult {
                        message_id: tool.message_id,
                        call_id: tool.id.clone(),
                    });
                };
                let Some(answered) = open.expected.get_mut(&tool.id) else {
                    return Err(InvalidHistory::OrphanToolResult {
                        message_id: tool.message_id,
                        call_id: tool.id.clone(),
                    });
                };
                if *answered {
                    return Err(InvalidHistory::DuplicateToolResult {
                        message_id: tool.message_id,
                        call_id: tool.id.clone(),
                    });
                }
                *answered = true;
            }
            Message::Assistant(assistant) if !assistant.tool_calls.is_empty() => {
                finish_batch(&mut batch)?;
                let mut expected = HashMap::with_capacity(assistant.tool_calls.len());
                for call in &assistant.tool_calls {
                    if expected.insert(call.id.clone(), false).is_some() {
                        return Err(InvalidHistory::DuplicateToolCall {
                            message_id: assistant.message_id,
                            call_id: call.id.clone(),
                        });
                    }
                }
                batch = Some(Batch {
                    assistant_message_id: assistant.message_id,
                    expected,
                });
            }
            Message::User(_)
            | Message::Assistant(_)
            | Message::Compaction(_)
            | Message::TaskNotice(_) => {
                finish_batch(&mut batch)?;
            }
        }
    }
    finish_batch(&mut batch)
}

/// The last summary's index, paired with where its protected tail begins —
/// one past its recorded `cutoff`, or the summary's own index when that
/// message is no longer in the history.
fn last_summary(messages: &[HistoryEntry]) -> Option<(usize, usize)> {
    let (summary_idx, summary) = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| is_compaction_summary(&entry.message))?;
    let tail_start = match &summary.message {
        Message::Compaction(compaction) => compaction.cutoff(),
        _ => None,
    }
    .and_then(|cutoff_id| {
        messages[..summary_idx]
            .iter()
            .rposition(|entry| entry.message.message_id() == cutoff_id)
            .map(|idx| idx + 1)
    })
    .unwrap_or(summary_idx);
    Some((summary_idx, tail_start))
}

pub(crate) fn is_compaction_summary(message: &Message) -> bool {
    matches!(message, Message::Compaction(compaction) if compaction.is_summary())
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_core::llm::{CompactionMessage, CompactionOutcome, MessageId, TurnId, UserMessage};

    // `pub(super)`: shared with `validation_tests`, a sibling module.
    pub(super) fn entry(message: Message) -> HistoryEntry {
        HistoryEntry::new(TurnId::from(MessageId::new()), message)
    }

    fn entry_in(turn: TurnId, message: Message) -> HistoryEntry {
        HistoryEntry::new(turn, message)
    }

    fn user(text: &str) -> HistoryEntry {
        entry(Message::User(UserMessage::text(MessageId::new(), text)))
    }

    fn user_in(turn: TurnId, text: &str) -> HistoryEntry {
        entry_in(
            turn,
            Message::User(UserMessage::text(MessageId::new(), text)),
        )
    }

    fn compaction(outcome: CompactionOutcome, content: &str) -> HistoryEntry {
        entry(Message::Compaction(CompactionMessage {
            message_id: MessageId::new(),
            outcome,
            content: content.to_string(),
            created_at: jiff::Timestamp::now(),
        }))
    }

    /// A summary whose `cutoff` names a message this history doesn't hold, so
    /// the view falls back to the summary's own position.
    fn summary(content: &str) -> HistoryEntry {
        compaction(
            CompactionOutcome::Summary {
                cutoff: MessageId::new(),
            },
            content,
        )
    }

    /// A compaction summary carrying a recorded `cutoff` pointing at
    /// `covers`, as an automatic mid-turn compaction would write.
    fn summary_covering(content: &str, covers: &HistoryEntry) -> HistoryEntry {
        compaction(
            CompactionOutcome::Summary {
                cutoff: covers.message.message_id(),
            },
            content,
        )
    }

    /// A failed compaction: transcript-only, and not a boundary.
    fn failure(content: &str) -> HistoryEntry {
        compaction(CompactionOutcome::Failed, content)
    }

    fn texts<'a>(entries: impl IntoIterator<Item = &'a HistoryEntry>) -> Vec<String> {
        entries
            .into_iter()
            .map(|entry| match &entry.message {
                Message::User(user) => user.first_text().unwrap_or_default().to_string(),
                Message::Compaction(compaction) => compaction.content.clone(),
                _ => unreachable!("these tests only build user and compaction messages"),
            })
            .collect()
    }

    #[test]
    fn a_thread_without_a_summary_is_shown_whole() {
        let history = vec![user("first"), user("second")];
        assert_eq!(texts(model_view(&history)), ["first", "second"]);
    }

    #[test]
    fn the_view_starts_at_the_summary_itself() {
        let history = vec![
            user("old"),
            user("/compact"),
            summary("summary"),
            user("next"),
        ];
        assert_eq!(texts(model_view(&history)), ["summary", "next"]);
    }

    #[test]
    fn the_last_summary_wins() {
        let history = vec![
            summary("first summary"),
            user("work"),
            summary("second summary"),
        ];
        assert_eq!(texts(model_view(&history)), ["second summary"]);
    }

    /// A failed compaction's record is transcript-only, so it changes nothing
    /// the model sees.
    #[test]
    fn a_failure_record_is_kept_out_of_the_view() {
        let history = vec![
            user("old"),
            user("/compact"),
            failure("the provider timed out"),
        ];
        assert_eq!(texts(model_view(&history)), ["old", "/compact"]);
    }

    /// Failure records stay hidden wherever they sit, even between the
    /// boundary and later conversation. The `/compact` line stays visible
    /// until a later successful summary's boundary sweeps it out.
    #[test]
    fn failure_records_between_boundary_and_talk_stay_hidden() {
        let history = vec![
            user("old"),
            summary("summary"),
            user("/compact"),
            failure("the provider timed out"),
            user("next"),
        ];
        assert_eq!(texts(model_view(&history)), ["summary", "/compact", "next"]);
    }

    #[test]
    fn an_empty_thread_has_an_empty_view() {
        assert!(model_view(&[]).next().is_none());
    }

    /// A mid-turn summary is appended after messages the current turn already
    /// produced, but its `cutoff` names an earlier one — those messages must
    /// reorder to *follow* the summary, not sit between it and the tail.
    #[test]
    fn a_mid_turn_summary_reorders_ahead_of_what_it_was_appended_after() {
        let earlier = user("earlier turn");
        let already_produced = user("current turn, before compaction landed");
        let summary = summary_covering("gist of the earlier turn", &earlier);
        let history = vec![
            earlier,
            already_produced.clone(),
            summary,
            user("current turn, after compaction landed"),
        ];
        assert_eq!(
            texts(model_view(&history)),
            [
                "gist of the earlier turn",
                "current turn, before compaction landed",
                "current turn, after compaction landed",
            ]
        );
    }

    /// A second summary with its own `cutoff` supersedes the first, and only
    /// messages after *its* cutoff reappear.
    #[test]
    fn a_later_summary_with_a_cutoff_still_wins_and_narrows_the_tail() {
        let root = user("root");
        let first_summary = summary_covering("first gist", &root);
        let protected = user("protected by the first summary");
        let second_summary = summary_covering("second gist", &protected);
        let history = vec![
            root,
            first_summary,
            protected,
            second_summary,
            user("fresh"),
        ];
        assert_eq!(texts(model_view(&history)), ["second gist", "fresh"]);
    }

    /// A summary whose `cutoff` names a message this history no longer holds
    /// falls back to the summary's own physical position.
    #[test]
    fn a_summary_with_an_unresolvable_cutoff_falls_back_to_its_own_position() {
        let history = vec![user("old"), summary("dangling summary"), user("next")];
        assert_eq!(texts(model_view(&history)), ["dangling summary", "next"]);
    }

    /// `model_view` never cares which turn a message belongs to.
    #[test]
    fn turn_tagging_does_not_affect_the_view() {
        let turn = TurnId::from(MessageId::new());
        let history = vec![user_in(turn, "a"), user_in(turn, "b")];
        assert_eq!(texts(model_view(&history)), ["a", "b"]);
    }
}

#[cfg(test)]
#[path = "message_view_validation_tests.rs"]
mod validation_tests;
