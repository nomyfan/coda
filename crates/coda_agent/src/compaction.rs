//! What a compacted thread shows the model.
//!
//! A compaction appends a summary to the thread and nothing else: the summary
//! message *is* the boundary, so rewind and fork cut it by the rule they
//! already apply to messages, and nothing here needs its own lifecycle. A
//! failed compaction appends its record too, but that record is transcript
//! material only: the view filters it out, so a failure changes what the
//! model sees exactly as little as the boundary rule already does.

use crate::agent::HistoryEntry;
use coda_core::llm::Message;

/// The `kind` of the summary a successful compaction writes. Only this kind
/// moves the boundary.
pub const COMPACTION_KIND: &str = "compaction";

/// The `kind` written when the summary could not be generated. It records what
/// happened for the transcript but is written transcript-only
/// (`visibility = Some(vec![Visibility::Transcript])`), so the model view never
/// pays for it. The boundary stays where it was.
pub const COMPACTION_FAILED_KIND: &str = "compaction_failed";

/// What the model is shown of `messages`: everything from the last compaction
/// summary onward, that summary included, minus the records that declared
/// themselves transcript-only (a failed compaction's record, for now).
///
/// A thread with no summary is shown whole — which covers every sub-agent
/// thread and every root thread before its first `/compact`, with no need to
/// ask which kind of thread this is.
pub fn view(messages: &[HistoryEntry]) -> impl Iterator<Item = &HistoryEntry> + '_ {
    let boundary = messages
        .iter()
        .rposition(|entry| is_compaction_summary(&entry.message))
        .unwrap_or(0);
    messages[boundary..]
        .iter()
        .filter(|entry| entry.message.visible_to_llm())
}

fn is_compaction_summary(message: &Message) -> bool {
    matches!(message, Message::Custom(custom) if custom.kind == COMPACTION_KIND)
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_core::llm::{CustomMessage, CustomRole, MessageId, TurnId, UserMessage, Visibility};

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
        custom_with_visibility(kind, content, None)
    }

    fn custom_with_visibility(
        kind: &str,
        content: &str,
        visibility: Option<Vec<Visibility>>,
    ) -> HistoryEntry {
        entry(Message::Custom(CustomMessage {
            message_id: MessageId::new(),
            kind: kind.to_string(),
            role: CustomRole::User,
            content: content.to_string(),
            created_at: jiff::Timestamp::now(),
            visibility,
        }))
    }

    fn texts<'a>(entries: impl IntoIterator<Item = &'a HistoryEntry>) -> Vec<String> {
        entries
            .into_iter()
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
    /// could not summarize, and without letting the record reach the model: it
    /// is written transcript-only, so a failure changes nothing the model sees.
    #[test]
    fn a_failure_record_is_kept_out_of_the_view() {
        let history = vec![
            user("old"),
            user("/compact"),
            custom_with_visibility(
                COMPACTION_FAILED_KIND,
                "the provider timed out",
                Some(vec![Visibility::Transcript]),
            ),
        ];
        assert_eq!(texts(view(&history)), ["old", "/compact"]);
    }

    /// Failure records are transcript-only wherever they sit: even between the
    /// boundary and real conversation they stay hidden, while the summary that
    /// actually moved the boundary keeps leading the view. The `/compact`
    /// command line stays — it is the user's own words, and only the next
    /// successful summary's boundary sweeps it out of view.
    #[test]
    fn failure_records_between_boundary_and_talk_stay_hidden() {
        let history = vec![
            user("old"),
            custom(COMPACTION_KIND, "summary"),
            user("/compact"),
            custom_with_visibility(
                COMPACTION_FAILED_KIND,
                "the provider timed out",
                Some(vec![Visibility::Transcript]),
            ),
            user("next"),
        ];
        assert_eq!(texts(view(&history)), ["summary", "/compact", "next"]);
    }

    /// The rule keys on the declared visibility, not on any particular kind:
    /// an ordinary custom message stays in the view.
    #[test]
    fn a_custom_message_without_visibility_restriction_stays_visible() {
        let history = vec![user("old"), custom("note", "a plain custom record")];
        assert_eq!(texts(view(&history)), ["old", "a plain custom record"]);
    }

    #[test]
    fn an_empty_thread_has_an_empty_view() {
        assert!(view(&[]).next().is_none());
    }
}
