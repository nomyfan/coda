use coda_core::tool::{Tool, ToolCallContext, ToolError, ToolResult};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::debug;

use crate::process::{CommandRun, run_command};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ShellToolParams {
    /// The shell command to execute.
    command: String,
    /// A short (5-10 word) description of what this command does, in active
    /// voice. For example: "List files in the current directory".
    description: String,
}

pub struct ShellTool {
    schema: Schema,
    description: String,
    cwd: String,
}

impl ShellTool {
    pub fn new(cwd: String) -> Self {
        let description =
            "Execute shell commands and return stdout and stderr. You are in a Unix environment."
                .to_string();
        let schema = schemars::schema_for!(ShellToolParams);
        debug!("ShellTool schema: {:?}", schema);

        ShellTool {
            schema,
            description,
            cwd,
        }
    }
}

impl Tool for ShellTool {
    type Parameters = ShellToolParams;
    type Output = String;

    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let cwd = self.cwd.clone();
        async move {
            debug!(description = %params.description, command = %params.command, "Executing shell command");
            // `shell` is the platform-agnostic tool name; `bash` is the current backend.
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(&params.command).current_dir(&cwd);

            let run = run_command(cmd, ctx.cancel).await.map_err(|e| {
                ToolError::ExecutionError(format!("Failed to execute command: {}", e))
            })?;

            let output = match run {
                CommandRun::Cancelled { stdout, stderr } => {
                    let stdout = String::from_utf8_lossy(&stdout);
                    let stderr = String::from_utf8_lossy(&stderr);
                    let mut reason =
                        String::from("Command was aborted by the user before completion.");
                    if !stdout.is_empty() {
                        reason.push_str(&format!("\nstdout (partial): {}", stdout));
                    }
                    if !stderr.is_empty() {
                        reason.push_str(&format!("\nstderr (partial): {}", stderr));
                    }
                    return Err(ToolError::Aborted(reason));
                }
                CommandRun::Completed(output) => output,
            };

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            if output.status.success() {
                Ok(format!("{}", stdout))
            } else {
                Ok(format!(
                    "exit code: {}\nstdout: {}\nstderr: {}",
                    output.status.code().unwrap_or(-1),
                    stdout,
                    stderr
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ShellTool {
        ShellTool::new(std::env::temp_dir().to_string_lossy().into_owned())
    }

    fn params(command: &str) -> ShellToolParams {
        ShellToolParams {
            command: command.to_string(),
            description: "test command".to_string(),
        }
    }

    fn process_alive(pid: i32) -> bool {
        // SAFETY: signal 0 only probes for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Kills a helper process this test spawned, even if an assertion fails.
    struct KillPidGuard(i32);

    impl Drop for KillPidGuard {
        fn drop(&mut self) {
            // SAFETY: plain signal syscall on the helper this test spawned.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }

    #[tokio::test]
    async fn completes_normally() {
        let out = tool()
            .execute(params("echo hello"), ToolCallContext::default())
            .await
            .unwrap();
        assert_eq!(out, "hello\n");
    }

    #[tokio::test]
    async fn reports_nonzero_exit() {
        let out = tool()
            .execute(params("echo oops >&2; exit 3"), ToolCallContext::default())
            .await
            .unwrap();
        assert!(out.starts_with("exit code: 3"), "unexpected output: {out}");
        assert!(out.contains("oops"), "unexpected output: {out}");
    }

    #[tokio::test]
    async fn cancel_kills_process_group_and_reports_partial_output() {
        let pidfile = std::env::temp_dir().join(format!("coda-shell-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);

        let ctx = ToolCallContext::default();
        let cancel = ctx.cancel.clone();
        // bash forks for the compound command, so $$ (bash) and $! (sleep)
        // are distinct processes in the same group.
        let command = format!(
            "echo partial-marker; sleep 37.51 & echo \"$$ $!\" > '{}'; wait",
            pidfile.display()
        );
        let fut = tokio::spawn(tool().execute(params(&command), ctx));

        // Wait for the command to be up (pidfile written), then cancel.
        let pids = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(content) = std::fs::read_to_string(&pidfile) {
                    let pids: Vec<i32> = content
                        .split_whitespace()
                        .filter_map(|p| p.parse().ok())
                        .collect();
                    if pids.len() == 2 {
                        break pids;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("command never wrote its pidfile");
        cancel.cancel();

        let result = fut.await.unwrap();
        let reason = match result {
            Err(ToolError::Aborted(reason)) => reason,
            other => panic!("expected Aborted, got {other:?}"),
        };
        assert!(
            reason.contains("partial-marker"),
            "partial stdout missing: {reason}"
        );

        // Both bash and its forked sleep must be gone. The forked sleep is
        // reaped asynchronously (by init) after the SIGKILL, so poll briefly.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if pids.iter().all(|&pid| !process_alive(pid)) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            let survivors: Vec<i32> = pids
                .iter()
                .copied()
                .filter(|&pid| process_alive(pid))
                .collect();
            // The group is led by the sentinel, not bash, so a group kill
            // keyed on these pids would miss; kill them directly.
            for &pid in &survivors {
                // SAFETY: plain signal syscall on processes this test spawned.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            panic!("processes survived cancellation: {survivors:?}");
        });

        let _ = std::fs::remove_file(&pidfile);
    }

    #[tokio::test]
    async fn pre_cancelled_context_never_runs_the_command() {
        let marker =
            std::env::temp_dir().join(format!("coda-shell-precancel-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        let ctx = ToolCallContext::default();
        ctx.cancel.cancel();
        let result = tool()
            .execute(params(&format!("touch '{}'", marker.display())), ctx)
            .await;
        assert!(
            matches!(result, Err(ToolError::Aborted(_))),
            "expected Aborted, got {result:?}"
        );

        // Give a wrongly-spawned bash time to leave its mark, then assert the
        // command truly never ran.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !marker.exists(),
            "pre-cancelled command still produced side effects"
        );
    }

    #[tokio::test]
    async fn sentinel_spawn_failure_fails_the_call_without_running_the_command() {
        let marker =
            std::env::temp_dir().join(format!("coda-shell-sentinel-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        crate::process::FAIL_SENTINEL.with(|f| f.set(true));
        let result = tool()
            .execute(
                params(&format!("touch '{}'", marker.display())),
                ToolCallContext::default(),
            )
            .await;
        crate::process::FAIL_SENTINEL.with(|f| f.set(false));

        assert!(
            matches!(result, Err(ToolError::ExecutionError(_))),
            "expected ExecutionError, got {result:?}"
        );

        // Fail-safe means fail-closed: with no sentinel there is no reliable
        // teardown, so the command must never have started.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !marker.exists(),
            "command ran despite the sentinel failing to spawn"
        );
    }

    #[tokio::test]
    async fn cancel_after_leader_exit_kills_lingering_children_and_salvages_output() {
        let pidfile =
            std::env::temp_dir().join(format!("coda-shell-linger-{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);

        let ctx = ToolCallContext::default();
        let cancel = ctx.cancel.clone();
        // bash exits immediately, but the backgrounded sleep inherits the
        // stdout pipe and keeps the drain open past the leader's exit.
        let command = format!(
            "sleep 37.81 & echo \"$!\" > '{}'; echo started",
            pidfile.display()
        );
        let fut = tokio::spawn(tool().execute(params(&command), ctx));

        let lingerer = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(content) = std::fs::read_to_string(&pidfile)
                    && let Ok(pid) = content.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("command never wrote its pidfile");
        let _cleanup = KillPidGuard(lingerer);
        // Give bash a beat to exit so the drain phase is what sees the abort.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();

        // Must settle promptly with the salvaged output, not hang on the pipe.
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), fut)
            .await
            .expect("cancellation hung on the lingering child's pipe")
            .unwrap();
        let reason = match result {
            Err(ToolError::Aborted(reason)) => reason,
            other => panic!("expected Aborted, got {other:?}"),
        };
        assert!(
            reason.contains("started"),
            "partial stdout missing: {reason}"
        );

        // The abort must have killed the lingering child, not just detached.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while process_alive(lingerer) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("lingering child survived cancellation");

        let _ = std::fs::remove_file(&pidfile);
    }

    #[tokio::test]
    async fn cancel_settles_promptly_when_a_descendant_escapes_the_group() {
        let ready = std::env::temp_dir().join(format!("coda-shell-escape-{}", std::process::id()));
        let _ = std::fs::remove_file(&ready);

        let ctx = ToolCallContext::default();
        let cancel = ctx.cancel.clone();
        // The perl helper setsids into its own session — escaping the group
        // kill — while inheriting the stdout pipe, so the pipe never EOFs on
        // its own and the bounded drain is what settles the abort. It writes
        // its own pid to the ready file; exec keeps that pid for the sleep.
        let command = format!(
            "perl -MPOSIX -e 'POSIX::setsid(); open my $f, \">\", $ARGV[0]; print $f $$; close $f; exec \"sleep\", \"37.71\"' '{}' & wait",
            ready.display()
        );
        let fut = tokio::spawn(tool().execute(params(&command), ctx));

        let escapee = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(content) = std::fs::read_to_string(&ready)
                    && let Ok(pid) = content.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("escaped descendant never signalled readiness");
        // The escapee survives the group kill by design; kill exactly it on
        // the way out, even if an assertion fails first.
        let _cleanup = KillPidGuard(escapee);
        cancel.cancel();

        // Must settle within the bounded drain, not when the sleep exits.
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), fut)
            .await
            .expect("cancellation hung on the escaped descendant's pipe")
            .unwrap();
        assert!(
            matches!(result, Err(ToolError::Aborted(_))),
            "expected Aborted, got {result:?}"
        );

        let _ = std::fs::remove_file(&ready);
    }
}
