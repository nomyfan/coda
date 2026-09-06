//! The process-group primitive shared by every caller that must be able to
//! kill a command's whole process tree.
//!
//! Both the foreground `shell` path in `coda_tools` and this crate's own
//! background task backend spawn through [`GroupedChild`]: a child pinned
//! inside a fresh, sentinel-led process group that can be SIGKILLed as a unit
//! and is never leaked if its owner is dropped mid-flight.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

/// How long a teardown path waits for the pipe readers after the group kill.
/// The kill normally EOFs the pipes at once; the deadline only matters when a
/// descendant escaped the process group (e.g. via setsid) and holds an
/// inherited pipe open. Kept short so an abort settles within the driver's
/// grace period; on expiry the readers are aborted and whatever partial
/// output they had buffered is lost.
pub const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// A child process running inside a fresh, sentinel-pinned process group,
/// with stdin null and stdout/stderr piped. The primitive shared by the
/// foreground `run_command` path and the background-task registry: spawn
/// into a killable group, SIGKILL the whole group on demand, and never leak
/// the group if the owner is dropped mid-flight.
pub struct GroupedChild {
    /// Pins the group so its numeric id stays ours for as long as we may
    /// still signal it — see [`spawn_sentinel`]. Killed by the group kill;
    /// kill_on_drop reaps it when this value drops.
    _sentinel: Child,
    /// `Some` until the group is killed or disarmed; taken so the group is
    /// never signalled twice (the id may be recycled once every member is
    /// reaped).
    pgid: Option<i32>,
    pub child: Child,
}

impl GroupedChild {
    /// Spawns `cmd` in a fresh process group. The sentinel spawns first: if
    /// it fails, the call fails before the command has run at all. The
    /// reverse order would leave a running command with no reliable way to
    /// kill it.
    pub fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
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
    pub fn kill_group(&mut self) {
        kill_group(self.pgid.take());
    }

    /// The command settled without a group kill; the group may empty out and
    /// its id be recycled, so never signal it again (including on drop).
    pub fn disarm(&mut self) {
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

// Failure injection for `spawn_sentinel`, exercising the fail-safe path where
// the user command must never start. Thread-local so parallel tests (each on
// its own thread, with current-thread runtimes) don't interfere. Behind a
// feature because the callers that exercise it live in other crates, whose
// `cfg(test)` does not reach this one.
#[cfg(feature = "test-hooks")]
thread_local! {
    static FAIL_SENTINEL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test hook: make the next [`GroupedChild::spawn`] on this thread fail to
/// spawn its group sentinel.
#[cfg(feature = "test-hooks")]
pub fn set_sentinel_failure(fail: bool) {
    FAIL_SENTINEL.with(|f| f.set(fail));
}

/// Spawns the process that leads and pins the group for one [`GroupedChild`].
/// Once the command's own processes are all reaped, an empty group's numeric
/// id could be recycled by the OS, and a later killpg would blast an
/// unrelated process group. The sentinel never exits on its own and holds
/// none of our pipes, so the group stays alive — and its id stays ours — for
/// as long as we may still signal it. Teardown paths kill it via killpg;
/// kill_on_drop reaps it when the [`GroupedChild`] drops.
fn spawn_sentinel() -> std::io::Result<Child> {
    #[cfg(feature = "test-hooks")]
    let program = if FAIL_SENTINEL.with(|f| f.get()) {
        "/nonexistent-coda-test-sentinel"
    } else {
        "sleep"
    };
    #[cfg(not(feature = "test-hooks"))]
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
