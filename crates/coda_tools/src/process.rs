//! Cancellation-aware child-process execution shared by the tools that shell
//! out (`shell`, `grep`, `glob`, `ls`).

use std::process::{Output, Stdio};
use std::time::Duration;

use coda_core::tool::CancellationToken;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::task::{AbortHandle, JoinHandle};

/// How long the cancellation path waits for the pipe readers after the group
/// kill. The kill normally EOFs the pipes at once; the deadline only matters
/// when a descendant escaped the process group (e.g. via setsid) and holds an
/// inherited pipe open. Kept short so an abort settles within the driver's
/// grace period; on expiry the readers are aborted and whatever partial
/// output they had buffered is lost.
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

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
/// command's process tree — see [`spawn_sentinel`]. On cancellation — whether
/// the command is still running or already exited with lingering children
/// holding the pipes open — the whole group is killed, and the pipes are
/// drained with a deadline. If the returned future is instead dropped
/// mid-flight, a guard kills the group and aborts the pipe readers, so
/// neither processes nor blocked reader tasks outlive the call unnoticed.
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

/// A child process running inside a fresh, sentinel-pinned process group,
/// with stdin null and stdout/stderr piped. The primitive shared by the
/// foreground [`run_command`] path and the background-task registry: spawn
/// into a killable group, SIGKILL the whole group on demand, and never leak
/// the group if the owner is dropped mid-flight.
pub(crate) struct GroupedChild {
    /// Pins the group so its numeric id stays ours for as long as we may
    /// still signal it — see [`spawn_sentinel`]. Killed by the group kill;
    /// kill_on_drop reaps it when this value drops.
    _sentinel: Child,
    /// `Some` until the group is killed or disarmed; taken so the group is
    /// never signalled twice (the id may be recycled once every member is
    /// reaped).
    pgid: Option<i32>,
    pub(crate) child: Child,
}

impl GroupedChild {
    /// Spawns `cmd` in a fresh process group. The sentinel spawns first: if
    /// it fails, the call fails before the command has run at all. The
    /// reverse order would leave a running command with no reliable way to
    /// kill it.
    pub(crate) fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        let sentinel = spawn_sentinel().map_err(|e| {
            std::io::Error::new(e.kind(), format!("failed to spawn group sentinel: {e}"))
        })?;
        let Some(pgid) = sentinel.id().map(|pid| pid as i32) else {
            // A freshly spawned child has a pid; bail out defensively if not.
            // kill_on_drop reaps the sentinel on return.
            return Err(std::io::Error::other("group sentinel pid unavailable"));
        };

        // The command joins the sentinel's group. The group is guaranteed
        // alive (the sentinel never exits on its own), so joining cannot
        // race.
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(pgid)
            .spawn()?;

        Ok(Self {
            _sentinel: sentinel,
            pgid: Some(pgid),
            child,
        })
    }

    /// Sends SIGKILL to the whole process group. Idempotent: a no-op once
    /// the group has been killed or disarmed.
    pub(crate) fn kill_group(&mut self) {
        kill_group(self.pgid.take());
    }

    /// The command settled without a group kill; the group may empty out and
    /// its id be recycled, so never signal it again (including on drop).
    pub(crate) fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for GroupedChild {
    fn drop(&mut self) {
        // Runs before the fields drop, so the sentinel still pins the group
        // at the killpg.
        kill_group(self.pgid.take());
    }
}

// Failure injection for `spawn_sentinel`, exercising the fail-safe path
// where the user command must never start. Thread-local so parallel tests
// (each on its own thread, with current-thread runtimes) don't interfere.
#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_SENTINEL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Spawns the process that leads and pins the group for one [`run_command`]
/// call. Once the command's own processes are all reaped, an empty group's
/// numeric id could be recycled by the OS, and a later killpg would blast an
/// unrelated process group. The sentinel never exits on its own and holds
/// none of our pipes, so the group stays alive — and its id stays ours — for
/// as long as we may still signal it. Teardown paths kill it via killpg;
/// kill_on_drop reaps it when the run_command future settles or is dropped.
fn spawn_sentinel() -> std::io::Result<Child> {
    #[cfg(test)]
    let program = if FAIL_SENTINEL.with(|f| f.get()) {
        "/nonexistent-coda-test-sentinel"
    } else {
        "sleep"
    };
    #[cfg(not(test))]
    let program = "sleep";

    Command::new(program)
        .arg("2147483647")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
}

/// Sends SIGKILL to the whole process group. A no-op for `None`.
fn kill_group(pgid: Option<i32>) {
    if let Some(pgid) = pgid {
        // SAFETY: plain signal syscall targeting a process group this module
        // spawned via `process_group(0)`.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
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
