//! Cancellation-aware child-process execution shared by the tools that shell
//! out (`shell`, `grep`, `glob`, `ls`).

use std::process::Output;

use coda_core::tool::CancellationToken;
use coda_process::{GroupedChild, PIPE_DRAIN_TIMEOUT};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::task::{AbortHandle, JoinHandle};

/// How a [`run_command`] invocation ended.
pub(crate) enum CommandOutcome {
    /// The command ran to completion (with any exit status).
    Completed(Output),
    /// Cancelled mid-flight: the process group was SIGKILLed and reaped; the
    /// pipes were drained best-effort so callers can salvage partial output.
    Cancelled { stdout: Vec<u8>, stderr: Vec<u8> },
}

/// Runs `cmd` in a fresh process group, racing it against `cancel`.
///
/// The group is led by a sentinel process spawned before the command, so the
/// group (and the ownership of its numeric id) outlives every member of the
/// command's process tree — see [`GroupedChild`]. On cancellation — whether
/// the command is still running or already exited with lingering children
/// holding the pipes open — the whole group is killed, and the pipes are
/// drained with a deadline. If the returned future is instead dropped
/// mid-flight, `GroupedChild`'s own Drop kills the group and a guard aborts
/// the pipe readers, so neither processes nor blocked reader tasks outlive
/// the call unnoticed.
pub(crate) async fn run_command(
    mut cmd: Command,
    cancel: CancellationToken,
) -> std::io::Result<CommandOutcome> {
    // A context that is already cancelled must not start the process at all:
    // a fast command could finish its side effects before the group kill.
    if cancel.is_cancelled() {
        return Ok(CommandOutcome::Cancelled {
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
    }

    let mut group = GroupedChild::spawn(&mut cmd)?;

    // Drain both pipes concurrently with wait(): a full pipe would block the
    // child forever. On cancellation the buffers collected so far become the
    // partial output.
    let mut stdout_pipe = group.child.stdout.take().expect("stdout is piped");
    let mut stderr_pipe = group.child.stderr.take().expect("stderr is piped");
    let mut stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf).await;
        buf
    });
    let mut stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    // If the future is dropped mid-flight, `group`'s Drop kills the process
    // group; this guard aborts the pipe readers so no blocked reader task
    // outlives the call unnoticed.
    let _readers = AbortReadersGuard([stdout_task.abort_handle(), stderr_task.abort_handle()]);

    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            // Kill the whole group, then reap the leader before we report
            // back. The pipes hit EOF, letting the reader tasks finish with
            // whatever was produced.
            group.kill_group();
            let _ = group.child.wait().await;
            None
        }
        status = group.child.wait() => Some(status),
    };

    // Cancelled before the leader exited: the group is dead, so the pipes EOF
    // at once — unless a descendant escaped the group (setsid) and holds one
    // open, which nothing will ever tear down. Bound the drain.
    let Some(status) = status else {
        return Ok(CommandOutcome::Cancelled {
            stdout: drain_reader(&mut stdout_task).await,
            stderr: drain_reader(&mut stderr_task).await,
        });
    };

    // A normal exit usually EOFs the pipes right away; a clean command whose
    // backgrounded children redirected their output settles here with those
    // children left alive. But a backgrounded child that inherited a pipe
    // holds the drain open indefinitely, so keep racing cancellation: on
    // abort, kill the group — the leader is gone, only such children remain
    // in it — and fall back to the bounded drain.
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            group.kill_group();
            Ok(CommandOutcome::Cancelled {
                stdout: drain_reader(&mut stdout_task).await,
                stderr: drain_reader(&mut stderr_task).await,
            })
        }
        bufs = async {
            (
                (&mut stdout_task).await.unwrap_or_default(),
                (&mut stderr_task).await.unwrap_or_default(),
            )
        } => {
            group.disarm();
            Ok(CommandOutcome::Completed(Output {
                status: status?,
                stdout: bufs.0,
                stderr: bufs.1,
            }))
        }
    }
}

/// Last-resort teardown if the [`run_command`] future is dropped mid-flight
/// (a caller that discards tool futures instead of cancelling): aborts the
/// pipe readers so they don't block forever on pipes nobody drains. The
/// process group itself is killed by [`GroupedChild`]'s own Drop.
struct AbortReadersGuard([AbortHandle; 2]);

impl Drop for AbortReadersGuard {
    fn drop(&mut self) {
        // No-ops for readers that already ran to completion or were aborted.
        for reader in &self.0 {
            reader.abort();
        }
    }
}

/// Await a pipe reader with a deadline, for the cancellation path. On expiry
/// the reader is aborted and whatever it buffered is lost — settling the
/// abort beats salvaging output from a pipe an escaped descendant may hold
/// open indefinitely.
async fn drain_reader(reader: &mut JoinHandle<Vec<u8>>) -> Vec<u8> {
    match tokio::time::timeout(PIPE_DRAIN_TIMEOUT, &mut *reader).await {
        Ok(buf) => buf.unwrap_or_default(),
        Err(_elapsed) => {
            reader.abort();
            Vec::new()
        }
    }
}
