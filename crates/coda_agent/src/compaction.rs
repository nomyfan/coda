//! What a compacted thread shows the model.
//!
//! A compaction appends a summary to the thread and nothing else: the summary
//! message *is* the boundary, so rewind and fork cut it by the rule they
//! already apply to messages, and nothing here needs its own lifecycle.

use crate::agent::HistoryEntry;
use coda_core::llm::Message;

/// The `kind` of the summary a successful compaction writes. Only this kind
/// moves the boundary.
pub const COMPACTION_KIND: &str = "compaction";

/// The `kind` written when the summary could not be generated. It records what
/// happened and is shown to the model, but leaves the boundary where it was.
pub const COMPACTION_FAILED_KIND: &str = "compaction_failed";

/// The slice of `messages` the model is shown: everything from the last
/// compaction summary onward, that summary included.
///
/// A thread with no summary is shown whole — which covers every sub-agent
/// thread and every root thread before its first `/compact`, with no need to
/// ask which kind of thread this is.
pub fn view(messages: &[HistoryEntry]) -> &[HistoryEntry] {
    let boundary = messages
        .iter()
        .rposition(|entry| is_compaction_summary(&entry.message))
        .unwrap_or(0);
    &messages[boundary..]
}

fn is_compaction_summary(message: &Message) -> bool {
    matches!(message, Message::Custom(custom) if custom.kind == COMPACTION_KIND)
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_core::llm::{CustomMessage, CustomRole, MessageId, TurnId, UserMessage};

    fn entry(message: Message) -> HistoryEntry {
        HistoryEntry {
            turn_id: TurnId::from(MessageId::new()),
            message,
        }
    }

    fn user(text: &str) -> HistoryEntry {
        entry(Message::User(UserMessage::text(MessageId::new(), text)))
    }

    fn custom(kind: &str, content: &str) -> HistoryEntry {
        entry(Message::Custom(CustomMessage {
            message_id: MessageId::new(),
            kind: kind.to_string(),
            role: CustomRole::User,
            content: content.to_string(),
            created_at: jiff::Timestamp::now(),
        }))
    }

    fn texts(entries: &[HistoryEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match &entry.message {
                Message::User(user) => user.first_text().unwrap_or_default().to_string(),
                Message::Custom(custom) => custom.content.clone(),
                _ => unreachable!("these tests only build user and custom messages"),
            })
            .collect()
    }

    #[test]
    fn a_thread_without_a_summary_is_shown_whole() {
        let history = vec![user("first"), user("second")];
        assert_eq!(texts(view(&history)), ["first", "second"]);
    }

    #[test]
    fn the_view_starts_at_the_summary_itself() {
        let history = vec![
            user("old"),
            user("/compact"),
            custom(COMPACTION_KIND, "summary"),
            user("next"),
        ];
        assert_eq!(texts(view(&history)), ["summary", "next"]);
    }

    #[test]
    fn the_last_summary_wins() {
        let history = vec![
            custom(COMPACTION_KIND, "first summary"),
            user("work"),
            custom(COMPACTION_KIND, "second summary"),
        ];
        assert_eq!(texts(view(&history)), ["second summary"]);
    }

    /// A failed compaction records what happened without hiding the history it
    /// could not summarize.
    #[test]
    fn a_failure_record_does_not_move_the_boundary() {
        let history = vec![
            user("old"),
            user("/compact"),
            custom(COMPACTION_FAILED_KIND, "the provider timed out"),
        ];
        assert_eq!(
            texts(view(&history)),
            ["old", "/compact", "the provider timed out"]
        );
    }

    #[test]
    fn an_empty_thread_has_an_empty_view() {
        assert!(view(&[]).is_empty());
    }
}
