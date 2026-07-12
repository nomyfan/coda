use std::sync::Arc;

use coda_core::tool::{Tool, ToolCallContext, ToolError, ToolResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::debug;

use crate::background::{BackgroundProcesses, TaskMeta};
use crate::process::{CommandOutcome, run_command};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ShellToolParams {
    /// The shell command to execute.
    command: String,
    /// A short (5-10 word) description of what this command does, in active
    /// voice. For example: "List files in the current directory".
    description: String,
    /// Run the command as a background task: the call returns immediately
    /// with a task id. Use task_output to read its output incrementally and
    /// task_kill to terminate it. Use this for long-running commands (dev
    /// servers, watchers, long builds).
    #[serde(default)]
    run_in_background: Option<bool>,
}

pub struct ShellTool {
    schema: serde_json::Value,
    description: String,
    cwd: String,
    agent_name: String,
    background: Arc<BackgroundProcesses>,
    /// Only when true does `run_in_background` appear in the schema (and
    /// take effect — the flag is ignored for agents not granted it).
    allow_background: bool,
}

impl ShellTool {
    pub fn new(
        cwd: String,
        agent_name: String,
        background: Arc<BackgroundProcesses>,
        allow_background: bool,
    ) -> Self {
        let description =
            "Execute shell commands and return stdout and stderr. You are in a Unix environment."
                .to_string();
        let mut schema = serde_json::to_value(schemars::schema_for!(ShellToolParams))
            .expect("shell schema serializes");
        if !allow_background
            && let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut())
        {
            props.remove("run_in_background");
        }
        debug!("ShellTool schema: {:?}", schema);

        ShellTool {
            schema,
            description,
            cwd,
            agent_name,
            background,
            allow_background,
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
        &self.schema
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let cwd = self.cwd.clone();
        let agent_name = self.agent_name.clone();
        let background = self.background.clone();
        let run_in_background = self.allow_background && params.run_in_background.unwrap_or(false);
        async move {
            debug!(description = %params.description, command = %params.command, "Executing shell command");
            // `shell` is the platform-agnostic tool name; `bash` is the current backend.
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(&params.command).current_dir(&cwd);

            if run_in_background {
                // The call settles now; the task lives outside tool-call
                // semantics (only task_kill / registry shutdown end it). An
                // already-aborted turn must not start work, mirroring the
                // foreground pre-cancellation check.
                if ctx.cancel.is_cancelled() {
                    return Err(ToolError::Aborted(
                        "Command was aborted by the user before it started.".into(),
                    ));
                }
                let id = background
                    .spawn(
                        cmd,
                        TaskMeta {
                            command: params.command.clone(),
                            description: params.description.clone(),
                            agent_name,
                        },
                    )
                    .await
                    .map_err(|e| {
                        ToolError::ExecutionError(format!("Failed to start background task: {e}"))
                    })?;
                return Ok(format!(
                    "Started background task {id}. Use task_output to read its \
                     output and task_kill to terminate it. You will be notified \
                     when it finishes."
                ));
            }

            let run = run_command(cmd, ctx.cancel).await.map_err(|e| {
                ToolError::ExecutionError(format!("Failed to execute command: {}", e))
            })?;

            let output = match run {
                CommandOutcome::Cancelled { stdout, stderr } => {
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
                CommandOutcome::Completed(output) => output,
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
        ShellTool::new(
            std::env::temp_dir().to_string_lossy().into_owned(),
            "coda".into(),
            Arc::new(BackgroundProcesses::new()),
            false,
        )
    }

    fn background_tool() -> ShellTool {
        ShellTool::new(
            std::env::temp_dir().to_string_lossy().into_owned(),
            "coda".into(),
            Arc::new(BackgroundProcesses::new()),
            true,
        )
    }

    fn params(command: &str) -> ShellToolParams {
        ShellToolParams {
            command: command.to_string(),
            description: "test command".to_string(),
            run_in_background: None,
        }
    }

    fn background_params(command: &str) -> ShellToolParams {
        ShellToolParams {
            run_in_background: Some(true),
            ..params(command)
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

    /// `run_in_background` appears in the schema only for agents granted it.
    #[test]
    fn run_in_background_is_gated_by_the_schema() {
        let gated = tool();
        let props = gated.parameter_schema()["properties"].as_object().unwrap();
        assert!(!props.contains_key("run_in_background"));

        let allowed = background_tool();
        let props = allowed.parameter_schema()["properties"]
            .as_object()
            .unwrap();
        assert!(props.contains_key("run_in_background"));
    }

    /// A backgrounded command settles immediately with the task id and the
    /// task is observable in the registry.
    #[tokio::test]
    async fn run_in_background_settles_immediately_with_a_task_id() {
        let background = Arc::new(BackgroundProcesses::new());
        let tool = ShellTool::new(
            std::env::temp_dir().to_string_lossy().into_owned(),
            "coda".into(),
            background.clone(),
            true,
        );
        let out = tool
            .execute(
                background_params("echo bg-marker; sleep 39.41"),
                ToolCallContext::default(),
            )
            .await
            .unwrap();
        let id = out
            .split_whitespace()
            .find(|w| w.starts_with("bg_"))
            .expect("task id in reply")
            .trim_end_matches('.');
        assert!(out.starts_with("Started background task bg_"), "{out}");
        assert!(
            background.summaries().borrow().iter().any(|s| s.id == id),
            "task not in registry"
        );
        background.shutdown().await;
    }

    /// A background task's lifetime is independent of the turn that started
    /// it: aborting the turn (the tool-call cancel token) must not kill it.
    #[tokio::test]
    async fn background_task_survives_turn_abort() {
        let background = Arc::new(BackgroundProcesses::new());
        let tool = ShellTool::new(
            std::env::temp_dir().to_string_lossy().into_owned(),
            "coda".into(),
            background.clone(),
            true,
        );
        let ctx = ToolCallContext::default();
        let cancel = ctx.cancel.clone();
        let out = tool
            .execute(background_params("sleep 39.61"), ctx)
            .await
            .unwrap();
        let id: crate::background::TaskId = out
            .split_whitespace()
            .find(|w| w.starts_with("bg_"))
            .expect("task id in reply")
            .trim_end_matches('.')
            .parse()
            .expect("valid task id");

        cancel.cancel();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let read = background
            .read(&id)
            .await
            .unwrap()
            .expect("task still known");
        assert!(
            read.status.is_running(),
            "turn abort must not kill the background task: {:?}",
            read.status
        );
        background.shutdown().await;
    }

    /// The flag is ignored (foreground execution) for agents whose schema
    /// does not expose it — a model hallucinating the parameter must not
    /// gain background execution.
    #[tokio::test]
    async fn run_in_background_is_ignored_when_not_allowed() {
        let out = tool()
            .execute(
                background_params("echo fg-marker"),
                ToolCallContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(out, "fg-marker\n");
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
