//! What a compaction may summarize, and the messages its answer becomes.
//!
//! The conversation is flattened to plain text rather than replayed as a
//! conversation — that avoids carrying tool definitions, truncated reasoning
//! continuations, and orphan tool results, for what is really a single
//! instruction: summarize this record.
//!
//! [`cutoff`] is the one rule both manual (idle) and automatic (mid-turn)
//! compaction use to decide what's new since the last one. Automatic
//! compaction prefers a complete turn boundary, then falls back to a complete
//! tool-batch boundary when the current turn itself has grown too large.

use crate::agent::HistoryEntry;
use crate::message_view;
use coda_core::llm::{
    ChatCompletionRequest, CompactionMessage, CompactionOutcome, ContentPart, Message, MessageId,
    RequestMessage, SystemMessage, ToolOutput, TurnId, UserMessage,
};

static COMPACTION_PROMPT: &str = include_str!("compaction-prompt.md");

/// The longest `instructions` a client may send. Generous for a line typed into
/// a composer, and small enough that it cannot crowd out the transcript.
pub const MAX_INSTRUCTIONS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cutoff {
    /// Entries to take from the full model view of the same history snapshot.
    pub model_view_len: usize,
    /// Physically latest included message; persisted as the summary boundary.
    pub coverage_message_id: MessageId,
}

struct VisibleEntry<'a> {
    history_index: usize,
    entry: &'a HistoryEntry,
}

impl<'a> From<(usize, &'a HistoryEntry)> for VisibleEntry<'a> {
    fn from((history_index, entry): (usize, &'a HistoryEntry)) -> Self {
        Self {
            history_index,
            entry,
        }
    }
}

fn prompt_growth(visible: &[VisibleEntry<'_>], turn: TurnId) -> Option<u64> {
    let mut previous = None;
    let mut growth = 0_u64;
    for item in visible {
        if item.entry.turn_id != turn {
            continue;
        }
        let Message::Assistant(assistant) = &item.entry.message else {
            continue;
        };
        let prompt_tokens = assistant.usage.as_ref()?.prompt_tokens;
        if let Some(previous) = previous {
            growth = growth.checked_add(u64::from(prompt_tokens.checked_sub(previous)?))?;
        }
        previous = Some(prompt_tokens);
    }
    Some(growth)
}

/// Chooses what a compaction may summarize without leaving an invalid tool
/// sequence behind. Automatic compaction prefers the boundary immediately
/// before the first visible message in `prefer_before_turn`, unless measured
/// growth within that turn already reaches `auto_compact_threshold_tokens`;
/// it then falls back to the newest safe boundary before `protect_from`.
/// Manual compaction omits all three policy inputs.
pub fn cutoff(
    messages: &[HistoryEntry],
    prefer_before_turn: Option<TurnId>,
    protect_from: Option<MessageId>,
    auto_compact_threshold_tokens: Option<u32>,
) -> Result<Option<Cutoff>, message_view::InvalidHistory> {
    let (summary, tail) = message_view::model_view_parts_indexed(messages);
    let summary = summary.map(VisibleEntry::from);
    let tail: Vec<_> = tail.map(VisibleEntry::from).collect();
    message_view::validate_messages(
        summary
            .iter()
            .chain(&tail)
            .map(|visible| &visible.entry.message),
    )?;

    let safe_cutoff_at = |position: usize| {
        let suffix = &tail[position + 1..];
        if message_view::validate_messages(suffix.iter().map(|visible| &visible.entry.message))
            .is_err()
        {
            return None;
        }

        let coverage = summary
            .iter()
            .chain(&tail[..=position])
            .max_by_key(|visible| visible.history_index)
            .expect("a cutoff position includes at least one message");
        let retained_from = suffix.iter().map(|visible| visible.history_index).min();
        // A reordered summary is always physically newer than the leftover
        // messages it protects; without this check, a later model_view tail
        // search could sweep an excluded leftover away too.
        if retained_from.is_some_and(|retained| coverage.history_index >= retained) {
            return None;
        }

        Some(Cutoff {
            model_view_len: usize::from(summary.is_some()) + position + 1,
            coverage_message_id: coverage.entry.message.message_id(),
        })
    };

    // Skip when the summary already covers this turn, or the turn starts at
    // the first tail entry (no position exists before the summary itself).
    if let Some(turn) = prefer_before_turn
        && summary.as_ref().is_none_or(|s| s.entry.turn_id != turn)
        && let Some(turn_start) = tail
            .iter()
            .position(|visible| visible.entry.turn_id == turn)
        && let Some(position) = turn_start.checked_sub(1)
        && let Some(candidate) = safe_cutoff_at(position)
    {
        let retaining_turn_is_too_large = auto_compact_threshold_tokens
            .zip(prompt_growth(&tail, turn))
            .is_some_and(|(threshold, growth)| growth >= u64::from(threshold));
        if !retaining_turn_is_too_large {
            return Ok(Some(candidate));
        }
    }

    let fallback_limit = match protect_from {
        None => tail.len(),
        // A stale or foreign id protects everything instead of silently
        // widening the prefix that compaction may replace.
        Some(message_id) => tail
            .iter()
            .position(|visible| visible.entry.message.message_id() == message_id)
            .unwrap_or(0),
    };
    Ok((0..fallback_limit).rev().find_map(safe_cutoff_at))
}

/// What prompted a compaction — only the human-facing wrapper text differs;
/// the prompt sent to the model is identical either way.
pub enum Trigger<'a> {
    /// A user-typed `/compact`, with whatever instructions they added.
    Manual { instructions: &'a str },
    /// The token-usage threshold was crossed; nobody asked for this.
    Auto,
}

/// The request that asks `model` to summarize `messages` — a model view
/// ([`message_view::model_view`] output) truncated at this compaction's
/// `cutoff`, so failure records never reach the summarizer.
pub fn summary_request<'a>(
    model: String,
    max_completion_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    messages: impl IntoIterator<Item = &'a HistoryEntry>,
    instructions: &str,
) -> ChatCompletionRequest {
    let mut task = String::new();
    if !instructions.is_empty() {
        task.push_str(
            "The user asked for this compaction and added the following. Follow it \
             where it narrows what to keep, but never at the cost of leaving the agent \
             unable to resume:\n",
        );
        task.push_str(instructions);
        task.push_str("\n\n");
    }
    task.push_str("<transcript>\n");
    task.push_str(&transcript(messages));
    task.push_str("</transcript>\n");
    ChatCompletionRequest {
        model,
        messages: vec![
            RequestMessage::System(SystemMessage(COMPACTION_PROMPT.to_string())),
            RequestMessage::User(UserMessage::text(MessageId::new(), task)),
        ],
        tools: Vec::new(),
        max_completion_tokens,
        temperature: None,
        reasoning_effort,
    }
}

/// The `/compact` line as the transcript records it — falls before the new
/// boundary, so the model never sees it; the summary carries the
/// instructions instead. Manual triggers only.
pub fn command_message(instructions: &str) -> Message {
    let text = if instructions.is_empty() {
        "/compact".to_string()
    } else {
        format!("/compact {instructions}")
    };
    Message::User(UserMessage::text(MessageId::new(), text))
}

/// The summary, which is also the new boundary: it records `cutoff` — the
/// last message it covers — so a later [`message_view::model_view`] knows
/// what it does and doesn't protect.
pub fn summary_message(cutoff: MessageId, trigger: Trigger, summary: &str) -> Message {
    let content = match trigger {
        Trigger::Manual { instructions } if !instructions.is_empty() => {
            format!("[compacted at the user's request: {instructions}]\n\n{summary}")
        }
        Trigger::Manual { .. } => summary.to_string(),
        Trigger::Auto => {
            format!(
                "[compacted automatically: token usage reached the configured limit]\n\n{summary}"
            )
        }
    };
    compaction(CompactionOutcome::Summary { cutoff }, content)
}

/// Recorded when no summary could be produced. Not a boundary, and
/// transcript-only, so the model view is untouched.
pub fn failure_message(reason: &str) -> Message {
    compaction(
        CompactionOutcome::Failed,
        format!("Compaction failed, so the conversation was left as it is: {reason}"),
    )
}

fn compaction(outcome: CompactionOutcome, content: String) -> Message {
    Message::Compaction(CompactionMessage {
        message_id: MessageId::new(),
        outcome,
        content,
        created_at: jiff::Timestamp::now(),
    })
}

fn transcript<'a>(messages: impl IntoIterator<Item = &'a HistoryEntry>) -> String {
    let mut out = String::new();
    for entry in messages {
        match &entry.message {
            Message::User(user) => {
                out.push_str("\n## User\n");
                for part in &user.parts {
                    match part {
                        ContentPart::Text { text } => {
                            out.push_str(text);
                            out.push('\n');
                        }
                        // Images cannot be flattened; noting the gap beats
                        // letting the summary imply the turn was text-only.
                        ContentPart::Image { .. } => out.push_str("[image attachment]\n"),
                    }
                }
            }
            Message::Assistant(assistant) => {
                out.push_str("\n## Assistant\n");
                if !assistant.content.is_empty() {
                    out.push_str(&assistant.content);
                    out.push('\n');
                }
                for call in &assistant.tool_calls {
                    out.push_str(&format!(
                        "\n### Tool call: {} {}\n",
                        call.name,
                        call.arguments.as_deref().unwrap_or("{}")
                    ));
                }
                if assistant.aborted {
                    out.push_str("\n[the user interrupted this]\n");
                }
            }
            Message::Tool(tool) => {
                let (label, body) = match &tool.output {
                    ToolOutput::Ok(text) => ("result", text),
                    ToolOutput::Err(reason) => ("error", reason),
                };
                out.push_str(&format!("\n### Tool {}: {}\n", label, tool.name));
                out.push_str(body);
                out.push('\n');
            }
            // An earlier summary, which is where this view begins.
            Message::Compaction(compaction) => {
                out.push_str("\n## Summary of the conversation so far\n");
                out.push_str(&compaction.content);
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
