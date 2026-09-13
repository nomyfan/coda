//! Cancellation-aware process execution with bounded stdout/stderr capture.
use std::sync::Arc;

use coda_core::output::{Channel, FINALIZE_TIMEOUT, IO_BLOCK_BYTES, OutputData, OutputStore};
use coda_core::tool::{CancellationToken, ToolCallContext, ToolError};
use coda_execution::{GroupedChild, PIPE_DRAIN_TIMEOUT};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub(crate) struct CommandOutcome {
    pub status: Option<std::process::ExitStatus>,
    pub output: OutputData,
}

pub(crate) fn output_store(ctx: &ToolCallContext) -> Arc<dyn OutputStore> {
    if let Some(store) = &ctx.output_store {
        return store.clone();
    }
    coda_output::Store::standalone()
}

pub(crate) fn preserve_error(
    ctx: &ToolCallContext,
    error: ToolError,
    output: OutputData,
) -> ToolError {
    match output {
        OutputData::Inline(text) | OutputData::Page { body: text, .. } => match error {
            ToolError::Aborted(reason) => ToolError::Aborted(format!("{reason}\n{text}")),
            ToolError::ExecutionError(reason) => {
                ToolError::ExecutionError(format!("{reason}\n{text}"))
            }
            error => error,
        },
        output => {
            ctx.preserve_output(output);
            error
        }
    }
}

pub(crate) async fn run_command(
    mut cmd: Command,
    ctx: ToolCallContext,
) -> std::io::Result<CommandOutcome> {
    if ctx.cancel.is_cancelled() {
        return Ok(CommandOutcome {
            status: None,
            output: OutputData::Inline(String::new()),
        });
    }
    let store = output_store(&ctx);
    let mut capture = tokio::select! {
        _ = ctx.cancel.cancelled() => return Ok(CommandOutcome { status: None, output: OutputData::Inline(String::new()) }),
        capture = store.begin(ctx.output_owner.clone(), vec![Channel::Stdout, Channel::Stderr], ctx.output_purpose.clone()) => capture.map_err(std::io::Error::other)?,
    };
    if ctx.cancel.is_cancelled() {
        return Ok(CommandOutcome {
            status: None,
            output: OutputData::Inline(String::new()),
        });
    }
    let mut group = GroupedChild::spawn(&mut cmd)?;
    let mut stdout = group.child.stdout.take().expect("piped stdout");
    let mut stderr = group.child.stderr.take().expect("piped stderr");
    let stop_drain = CancellationToken::new();
    let collector_stop = stop_drain.clone();
    let mut reader = tokio::spawn(async move {
        let mut out_open = true;
        let mut err_open = true;
        let mut out_block = [0u8; IO_BLOCK_BYTES / 2];
        let mut err_block = [0u8; IO_BLOCK_BYTES / 2];
        while out_open || err_open {
            let (channel, read) = tokio::select! {
                _ = collector_stop.cancelled() => { capture.fail(coda_core::output::StorageFailure::Incomplete); break; },
                read = stdout.read(&mut out_block), if out_open => (Channel::Stdout, read),
                read = stderr.read(&mut err_block), if err_open => (Channel::Stderr, read),
            };
            match read {
                Err(_) => {
                    capture.fail(coda_core::output::StorageFailure::Io);
                    if channel == Channel::Stdout {
                        out_open = false;
                    } else {
                        err_open = false;
                    }
                }
                Ok(0) => {
                    if channel == Channel::Stdout {
                        out_open = false;
                    } else {
                        err_open = false;
                    }
                }
                Ok(count) => {
                    let bytes = if channel == Channel::Stdout {
                        &out_block[..count]
                    } else {
                        &err_block[..count]
                    };
                    capture.append(channel, bytes.to_vec()).await;
                }
            }
        }
        capture
            .finish(tokio::time::Instant::now() + FINALIZE_TIMEOUT)
            .await
    });
    let _guard = ReaderGuard(reader.abort_handle());
    let mut status = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => { group.kill_group(); let _ = group.child.wait().await; None }
        status = group.child.wait() => Some(status?),
    };
    let mut drain_deadline = status
        .is_none()
        .then(|| tokio::time::Instant::now() + PIPE_DRAIN_TIMEOUT);
    let output = loop {
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled(), if status.is_some() => {
                group.kill_group();
                status = None;
                drain_deadline = Some(tokio::time::Instant::now() + PIPE_DRAIN_TIMEOUT);
            }
            output = &mut reader => break output,
            _ = async {
                match drain_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                group.kill_group();
                stop_drain.cancel();
                break reader.await;
            }
        }
    }
    .map_err(std::io::Error::other)?;
    group.disarm();
    Ok(CommandOutcome { status, output })
}

struct ReaderGuard(tokio::task::AbortHandle);
impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}
