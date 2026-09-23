use coda_core::output::preview::leading_lines;
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
) -> Result<RenderedOutput, OutputError> {
    let output = match output {
        // A lease only exists for a script's call, which never renders.
        OutputData::Page {
            body, references, ..
        } => {
            if body.len() > bytes {
                return Err(OutputError::PageLimit(
                    "page exceeds its assigned delivery budget".into(),
                ));
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
                .retain(
                    owner,
                    body,
                    bytes,
                    tokio::time::Instant::now() + FINALIZE_TIMEOUT,
                )
                .await
        }
        output => output,
    };
    match output {
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
            let (shown, _) =
                OutputPreview::of(Channel::Result, &body, bytes).render(bytes.saturating_sub(192));
            Ok(RenderedOutput {
                delivery_error: false,
                body: format!(
                    "{shown}\n[output truncated: longer than the {bytes}-byte output limit]\n[full output was not saved: storage is unavailable]"
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
    preview: &OutputPreview,
    references: &[OutputRef],
    failure: Option<StorageFailure>,
    report_ok: Option<bool>,
    bytes: usize,
) -> Result<String, OutputError> {
    let head = report_ok.map_or(String::new(), |ok| format!("ok: {ok}\n"));
    let truncated = format!("\n[output truncated: longer than the {bytes}-byte output limit]");
    let mut saved = describe_saved(references, failure.as_ref());
    if !saved.is_empty() {
        saved.insert(0, '\n');
    }
    let fixed = head.len() + truncated.len() + saved.len();
    if fixed > bytes {
        return Err(OutputError::MetadataLimit);
    }
    let (shown, cut) = preview.render(bytes - fixed);
    let truncated = if cut { truncated.as_str() } else { "" };
    Ok(format!("{head}{shown}{truncated}{saved}"))
}

/// Rebound an existing message without changing execution outcome or read receipts.
pub async fn bound_tool(
    store: &dyn OutputStore,
    owner: OutputOwner,
    tool: &mut coda_core::llm::ToolMessage,
    bytes: usize,
) -> Result<(), OutputError> {
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
                bytes,
                tokio::time::Instant::now() + FINALIZE_TIMEOUT,
            )
            .await;
        let rendered = render(store, owner, output, bytes).await?;
        tool.output_refs = rendered.references;
        rendered.body
    } else {
        // The body is an earlier render of the saved files. Its own line
        // markers stay accurate only while whole, so keep its leading lines.
        const NOTE: &str =
            "[... rest of this earlier preview omitted to fit a smaller output limit ...]";
        let saved = describe_saved(&tool.output_refs, None);
        let earlier = body
            .strip_suffix(saved.as_str())
            .unwrap_or(body)
            .trim_end_matches('\n');
        let fixed = NOTE.len() + 1 + saved.len() + 1;
        if fixed > bytes {
            return Err(OutputError::MetadataLimit);
        }
        let kept = leading_lines(earlier, bytes - fixed);
        [kept, NOTE, &saved]
            .into_iter()
            .filter(|piece| !piece.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
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
        let text = render_saved(
            &OutputPreview::of(Channel::Stdout, &preview, 4096),
            &[reference(2000, 2000, None)],
            None,
            None,
            600,
        )
        .unwrap();
        assert!(text.len() <= 600);
        assert!(text.starts_with("aaa"));
        assert!(text.contains(" [line 1 truncated: "), "{text}");
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
        let text = render_saved(
            &OutputPreview::of(Channel::Stdout, "abc", 64),
            &[saved],
            None,
            None,
            4096,
        )
        .unwrap();
        assert!(text.contains("stdout.txt"));
        assert!(!text.contains("stderr"));
    }

    #[test]
    fn partial_saves_and_missing_saves_say_why() {
        let partial = render_saved(
            &OutputPreview::of(Channel::Stdout, "abc", 64),
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
        let missing = render_saved(
            &OutputPreview::of(Channel::Stdout, "abc", 64),
            &[],
            Some(StorageFailure::Io),
            Some(true),
            4096,
        )
        .unwrap();
        assert_eq!(
            missing,
            "ok: true\nabc\n[full output was not saved: writing to disk failed]"
        );
    }
}
