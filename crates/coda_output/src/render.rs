use crate::preview::Preview;
use coda_core::output::*;
use std::sync::Arc;

pub struct RenderedOutput {
    pub delivery_error: bool,
    pub body: String,
    pub references: Vec<OutputRef>,
    pub lease: Option<Arc<dyn OutputBuffer>>,
}

/// The paths and metadata always remain whole. Only preview text is reduced.
pub async fn render(
    store: &dyn OutputStore,
    owner: OutputOwner,
    output: OutputData,
    bytes: usize,
) -> Result<RenderedOutput, String> {
    let output = match output {
        OutputData::Buffered(_) => {
            return Err("OUTPUT_DELIVERY: programmatic buffer reached the model boundary".into());
        }
        OutputData::Page { body, references } => {
            if body.len() > bytes {
                return Err("OUTPUT_PAGE_LIMIT: page exceeds its assigned delivery budget".into());
            }
            return Ok(RenderedOutput {
                delivery_error: false,
                body,
                references,
                lease: None,
            });
        }
        OutputData::Inline(body) if body.len() <= bytes => {
            return Ok(RenderedOutput {
                delivery_error: false,
                body,
                references: vec![],
                lease: None,
            });
        }
        OutputData::Inline(body) => {
            store
                .retain(owner, body, tokio::time::Instant::now() + FINALIZE_TIMEOUT)
                .await
        }
        output => output,
    };
    match output {
        OutputData::Buffered(_) => unreachable!(),
        OutputData::Captured(output) => {
            let references: Vec<_> = output.reference.into_iter().collect();
            let best = render_saved(
                &output.preview,
                &references,
                output.failure,
                output.report_ok,
                bytes,
            )?;
            Ok(RenderedOutput {
                delivery_error: false,
                body: best,
                references,
                lease: Some(output.buffer),
            })
        }
        OutputData::Inline(body) | OutputData::Page { body, .. } => {
            // Even a store initialization failure cannot bypass the model limit.
            let mut preview = Preview::new(bytes.saturating_sub(192));
            preview.append(body.as_bytes());
            Ok(RenderedOutput {
                delivery_error: false,
                body: format!(
                    "{}\n[output truncated: longer than the {bytes}-byte output limit]\n[full output was not saved: storage is unavailable]",
                    preview.text()
                ),
                references: vec![],
                lease: None,
            })
        }
    }
}

/// Fits a preview and the saved-file description into `bytes` as plain text.
/// The file lines always remain whole; only the preview is shortened.
pub fn render_saved(
    preview: &str,
    references: &[OutputRef],
    failure: Option<StorageFailure>,
    report_ok: Option<bool>,
    bytes: usize,
) -> Result<String, String> {
    let saved = describe_saved(references, failure.as_ref());
    let head = report_ok.map_or(String::new(), |ok| format!("ok: {ok}\n"));
    let captured: u64 = references
        .iter()
        .flat_map(|reference| &reference.channels)
        .map(|channel| channel.captured_bytes)
        .sum();
    let compose = |shown: &str, cut: bool| {
        let mut text = format!("{head}{shown}");
        if cut {
            text.push_str(&format!(
                "\n[output truncated: longer than the {bytes}-byte output limit]"
            ));
        } else if captured > shown.len() as u64 {
            text.push_str("\n[output truncated: only the start and end were kept in memory]");
        }
        if !saved.is_empty() {
            text.push('\n');
            text.push_str(&saved);
        }
        text
    };
    if compose("", true).len() > bytes {
        return Err("OUTPUT_METADATA_LIMIT: response cannot fit complete output paths".into());
    }
    let whole = compose(preview, false);
    if whole.len() <= bytes {
        return Ok(whole);
    }
    let mut low = 0;
    let mut high = preview.len();
    let mut best = compose("", true);
    while low <= high {
        let capacity = low + (high - low) / 2;
        let mut bounded = Preview::new(capacity);
        bounded.append(preview.as_bytes());
        let candidate = compose(&bounded.text(), true);
        if candidate.len() <= bytes {
            best = candidate;
            low = capacity + 1;
        } else if capacity == 0 {
            break;
        } else {
            high = capacity - 1;
        }
    }
    Ok(best)
}

/// Rebound an existing message without changing execution outcome or read receipts.
pub async fn bound_tool(
    store: &dyn OutputStore,
    owner: OutputOwner,
    tool: &mut coda_core::llm::ToolMessage,
    bytes: usize,
) -> Result<(), String> {
    let body = match &mut tool.output {
        coda_core::llm::ToolOutput::Ok(body) | coda_core::llm::ToolOutput::Err(body) => body,
    };
    if body.len() <= bytes {
        return Ok(());
    }
    *body = if tool.output_refs.is_empty() {
        let output = store
            .retain_source(
                owner.clone(),
                tool.message_id,
                std::mem::take(body),
                tokio::time::Instant::now() + FINALIZE_TIMEOUT,
            )
            .await;
        let rendered = render(store, owner, output, bytes).await?;
        tool.output_refs = rendered.references;
        rendered.body
    } else {
        render_saved(body, &tool.output_refs, None, None, bytes)?
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(captured: u64, saved: u64, failure: Option<StorageFailure>) -> OutputRef {
        OutputRef {
            id: OutputId::default(),
            channels: vec![OutputChannelRef {
                channel: Channel::Stdout,
                path: "/out/objects/x/stdout.txt".into(),
                captured_bytes: captured,
                saved_bytes: saved,
            }],
            complete: failure.is_none(),
            failure,
            sealed_at: jiff::Timestamp::UNIX_EPOCH,
            expires_at: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    #[test]
    fn oversized_output_is_plain_text_with_reason_and_paths() {
        let preview = "a".repeat(2000);
        let text = render_saved(&preview, &[reference(2000, 2000, None)], None, None, 600).unwrap();
        assert!(text.len() <= 600);
        assert!(text.starts_with("aaa"));
        assert!(text.contains("bytes omitted"));
        assert!(text.contains("\n[output truncated: longer than the 600-byte output limit]"));
        assert!(text.contains("\n[stdout saved to /out/objects/x/stdout.txt (2000 bytes)]"));
        assert!(text.ends_with("[saved output expires at 1970-01-01T00:00:00Z]"));
    }

    #[test]
    fn empty_channels_are_not_listed() {
        let mut saved = reference(2000, 2000, None);
        saved.channels.push(OutputChannelRef {
            channel: Channel::Stderr,
            path: "/out/objects/x/stderr.txt".into(),
            captured_bytes: 0,
            saved_bytes: 0,
        });
        let text = render_saved("abc", &[saved], None, None, 4096).unwrap();
        assert!(text.contains("stdout.txt"));
        assert!(!text.contains("stderr"));
    }

    #[test]
    fn partial_saves_and_missing_saves_say_why() {
        let partial = render_saved(
            "abc",
            &[reference(9000, 100, Some(StorageFailure::SessionQuota))],
            None,
            None,
            4096,
        )
        .unwrap();
        assert!(partial.contains("(100 of 9000 bytes)"));
        assert!(
            partial.contains("[saved output is incomplete: the session disk quota was reached]")
        );
        let missing = render_saved("abc", &[], Some(StorageFailure::Io), Some(true), 4096).unwrap();
        assert_eq!(
            missing,
            "ok: true\nabc\n[full output was not saved: writing to disk failed]"
        );
    }
}
