//! What a thread shows the model — the model view — and the kinds the
//! compaction machinery writes.
//!
//! [`model_view`] is the model's window on a thread: the last compaction
//! summary (if any) leading, followed by everything its recorded `cutoff`
//! excludes, in original order, minus the records that declared themselves
//! transcript-only. The summary always leads the view because a compaction
//! must be appended after whatever it protects (storage only ever grows at
//! the tail — see `coda_agent::compaction`), even when what it protects is an
//! in-progress turn's own messages written before the summary landed; the
//! `cutoff` field is what tells the view where the summary's coverage
//! actually ends, independent of where it was physically appended. A summary
//! with no recorded `cutoff` (every one written before that field existed)
//! falls back to its own physical position, which is what it always meant.
//! A failed compaction appends its record too, but that record is transcript
//! material only: the model view filters it out, so a failure changes what
//! the model sees exactly as little as the boundary rule already does.

use crate::agent::HistoryEntry;
use coda_core::llm::Message;

/// The `kind` of the summary a successful compaction writes. Only this kind
/// moves the boundary.
pub const COMPACTION_KIND: &str = "compaction";

/// The `kind` written when the summary could not be generated. It records what
/// happened for the transcript but is written without a role (`role: None`),
/// so the model view never pays for it. The boundary stays where it was.
pub const COMPACTION_FAILED_KIND: &str = "compaction_failed";

/// What the model is shown of `messages`: the last compaction summary (if
/// any) leading, followed by everything its `cutoff` excludes in original
/// order, minus the records that declared themselves transcript-only (a
/// failed compaction's record, for now).
///
/// A thread with no summary is shown whole — which covers every sub-agent
/// thread and every root thread before its first compaction, with no need to
/// ask which kind of thread this is.
pub fn model_view(messages: &[HistoryEntry]) -> impl Iterator<Item = &HistoryEntry> + '_ {
    let ordered: Box<dyn Iterator<Item = &HistoryEntry> + '_> = match last_summary(messages) {
        // No allocation for the common case: a thread with no summary yet
        // (every sub-agent thread, and every root thread before its first
        // compaction) is shown whole via a plain iterator over the slice.
        None => Box::new(messages.iter()),
        Some((summary_idx, tail_start)) => {
            let summary = &messages[summary_idx];
            Box::new(std::iter::once(summary).chain(
                messages[tail_start..].iter().filter(move |entry| {
                    entry.message.message_id() != summary.message.message_id()
                }),
            ))
        }
    };
    ordered.filter(|entry| entry.message.visible_to_model())
}

/// The last compaction summary's own index, paired with where its protected
/// tail begins — one past its recorded `cutoff` resolved to a position, or,
/// for a summary with none recorded, the summary's own index (so the tail
/// starts there and the explicit `once(summary)` lead above supplies it once).
fn last_summary(messages: &[HistoryEntry]) -> Option<(usize, usize)> {
    let (summary_idx, summary) = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| is_compaction_summary(&entry.message))?;
    let tail_start = match &summary.message {
        Message::Custom(custom) => custom.cutoff,
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

    fn entry_in(turn: TurnId, message: Message) -> HistoryEntry {
        HistoryEntry {
            turn_id: turn,
            message,
        }
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

    /// A custom message with a role: model-visible, like a summary.
    fn custom(kind: &str, content: &str) -> HistoryEntry {
        entry(Message::Custom(CustomMessage {
            message_id: MessageId::new(),
            kind: kind.to_string(),
            role: Some(CustomRole::User),
            content: content.to_string(),
            created_at: jiff::Timestamp::now(),
            cutoff: None,
        }))
    }

    /// A compaction summary carrying a recorded `cutoff` pointing at
    /// `covers`, as an automatic mid-turn compaction would write.
    fn summary_covering(content: &str, covers: &HistoryEntry) -> HistoryEntry {
        entry(Message::Custom(CustomMessage {
            message_id: MessageId::new(),
            kind: COMPACTION_KIND.to_string(),
            role: Some(CustomRole::User),
            content: content.to_string(),
            created_at: jiff::Timestamp::now(),
            cutoff: Some(covers.message.message_id()),
        }))
    }

    /// A role-less custom message: transcript-only, like a failure record.
    fn custom_transcript_only(kind: &str, content: &str) -> HistoryEntry {
        entry(Message::Custom(CustomMessage {
            message_id: MessageId::new(),
            kind: kind.to_string(),
            role: None,
            content: content.to_string(),
            created_at: jiff::Timestamp::now(),
            cutoff: None,
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
        assert_eq!(texts(model_view(&history)), ["first", "second"]);
    }

    #[test]
    fn the_view_starts_at_the_summary_itself() {
        let history = vec![
            user("old"),
            user("/compact"),
            custom(COMPACTION_KIND, "summary"),
            user("next"),
        ];
        assert_eq!(texts(model_view(&history)), ["summary", "next"]);
    }

    #[test]
    fn the_last_summary_wins() {
        let history = vec![
            custom(COMPACTION_KIND, "first summary"),
            user("work"),
            custom(COMPACTION_KIND, "second summary"),
        ];
        assert_eq!(texts(model_view(&history)), ["second summary"]);
    }

    /// A failed compaction records what happened without hiding the history it
    /// could not summarize, and without letting the record reach the model: it
    /// is written transcript-only, so a failure changes nothing the model sees.
    #[test]
    fn a_failure_record_is_kept_out_of_the_view() {
        let history = vec![
            user("old"),
            user("/compact"),
            custom_transcript_only(COMPACTION_FAILED_KIND, "the provider timed out"),
        ];
        assert_eq!(texts(model_view(&history)), ["old", "/compact"]);
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
            custom_transcript_only(COMPACTION_FAILED_KIND, "the provider timed out"),
            user("next"),
        ];
        assert_eq!(texts(model_view(&history)), ["summary", "/compact", "next"]);
    }

    /// The rule keys on the role, not on any particular kind: a custom message
    /// with a role stays in the view.
    #[test]
    fn a_custom_message_with_a_role_stays_visible() {
        let history = vec![user("old"), custom("note", "a plain custom record")];
        assert_eq!(
            texts(model_view(&history)),
            ["old", "a plain custom record"]
        );
    }

    #[test]
    fn an_empty_thread_has_an_empty_view() {
        assert!(model_view(&[]).next().is_none());
    }

    /// A summary written mid-turn is appended after messages the current turn
    /// already produced, but its `cutoff` names an earlier message — so those
    /// already-produced messages must stay in the view, reordered to *follow*
    /// the summary rather than sit between it and the tail where storage
    /// physically placed them.
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

    /// A second, later summary that also carries a `cutoff` supersedes the
    /// first, and only messages after *its* cutoff reappear — the first
    /// summary's own protected tail is compacted away in turn once something
    /// newer covers it.
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

    /// A summary with no recorded `cutoff` — every summary written before the
    /// field existed — falls back to its own physical position, so an
    /// already-persisted thread's view is unchanged by this rule existing.
    #[test]
    fn a_summary_with_no_recorded_cutoff_falls_back_to_its_own_position() {
        let history = vec![
            user("old"),
            custom(COMPACTION_KIND, "legacy summary"),
            user("next"),
        ];
        assert_eq!(texts(model_view(&history)), ["legacy summary", "next"]);
    }

    /// `is_compaction_summary` and `last_summary` operate on `turn_id`-tagged
    /// entries too — the compaction machinery cares which turn a message
    /// belongs to, `model_view` never does.
    #[test]
    fn turn_tagging_does_not_affect_the_view() {
        let turn = TurnId::from(MessageId::new());
        let history = vec![user_in(turn, "a"), user_in(turn, "b")];
        assert_eq!(texts(model_view(&history)), ["a", "b"]);
    }
}
