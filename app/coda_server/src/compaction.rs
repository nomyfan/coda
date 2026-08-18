//! Building the request that summarizes a conversation, and the two messages
//! its answer becomes.
//!
//! The conversation is flattened to plain text rather than replayed as a
//! conversation. That drops three provider-compatibility problems at once — the
//! request would otherwise have to carry the full tool definitions behind every
//! `tool_calls` in the history, preserve truncated reasoning continuations, and
//! guarantee no tool result is left without its call. None of it earns its keep
//! for what is really a single instruction: summarize this record.

use coda_agent::HistoryEntry;
use coda_agent::message_view::{COMPACTION_FAILED_KIND, COMPACTION_KIND};
use coda_core::llm::{
    ChatCompletionRequest, ContentPart, CustomMessage, CustomRole, Message, MessageId,
    RequestMessage, SystemMessage, ToolOutput, UserMessage,
};

static COMPACTION_PROMPT: &str = include_str!("compaction-prompt.md");

/// The longest `instructions` a client may send. Generous for a line typed into
/// a composer, and small enough that it cannot crowd out the transcript.
pub const MAX_INSTRUCTIONS: usize = 4096;

/// The request that asks `model` to summarize `messages`. `messages` is the
/// model view — `coda_agent::message_view::model_view` output — so failure
/// records, which are transcript-only, never reach the summarizer.
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

/// The `/compact` line as the transcript records it. This is the only trace of
/// what the user typed: it falls before the new boundary, so the model never
/// sees it — the summary carries the instructions instead.
pub fn command_message(instructions: &str) -> Message {
    let text = if instructions.is_empty() {
        "/compact".to_string()
    } else {
        format!("/compact {instructions}")
    };
    Message::User(UserMessage::text(MessageId::new(), text))
}

/// The summary, which is also the new boundary. It restates the user's
/// instructions because the message carrying them is about to fall out of view.
pub fn summary_message(instructions: &str, summary: &str) -> Message {
    let content = if instructions.is_empty() {
        summary.to_string()
    } else {
        format!("[compacted at the user's request: {instructions}]\n\n{summary}")
    };
    custom(COMPACTION_KIND, Some(CustomRole::User), content)
}

/// What is recorded when no summary could be produced. It is *not* a boundary
/// and it is transcript-only (no role), so the model view is untouched — the
/// transcript keeps it as an honest account of why the user's request did
/// nothing.
pub fn failure_message(reason: &str) -> Message {
    custom(
        COMPACTION_FAILED_KIND,
        None,
        format!("Compaction failed, so the conversation was left as it is: {reason}"),
    )
}

fn custom(kind: &str, role: Option<CustomRole>, content: String) -> Message {
    Message::Custom(CustomMessage {
        message_id: MessageId::new(),
        kind: kind.to_string(),
        role,
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
            Message::Custom(custom) => {
                out.push_str(&format!("\n## {}\n", custom.kind));
                out.push_str(&custom.content);
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
