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
            let mut preview = Preview::new(bytes.saturating_sub(128));
            preview.append(body.as_bytes());
            Ok(RenderedOutput {
                delivery_error: false,
                body: format!(
                    "{}\n[Complete output was not retained: storage unavailable]",
                    preview.text()
                ),
                references: vec![],
                lease: None,
            })
        }
    }
}

pub fn render_saved(
    preview: &str,
    references: &[OutputRef],
    failure: Option<StorageFailure>,
    report_ok: Option<bool>,
    bytes: usize,
) -> Result<String, String> {
    let mut envelope =
        serde_json::json!({ "preview": "", "output_refs": references, "storage_failure": failure });
    if let Some(ok) = report_ok {
        envelope["ok"] = ok.into();
        envelope.as_object_mut().unwrap().remove("preview");
        envelope["value_preview"] = "".into();
    }
    let preview_key = if report_ok.is_some() {
        "value_preview"
    } else {
        "preview"
    };
    let empty = serde_json::to_string(&envelope)
        .map_err(|e| e.to_string())?
        .len();
    if empty > bytes {
        return Err("OUTPUT_METADATA_LIMIT: response cannot fit complete output paths".into());
    }
    let mut low = 0;
    let mut high = preview.len();
    let mut best = serde_json::to_string(&envelope).unwrap();
    while low <= high {
        let capacity = low + (high - low) / 2;
        let mut bounded = Preview::new(capacity);
        bounded.append(preview.as_bytes());
        envelope[preview_key] = bounded.text().into();
        let candidate = serde_json::to_string(&envelope).unwrap();
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
