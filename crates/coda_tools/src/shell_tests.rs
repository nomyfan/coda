use super::*;
use std::str::FromStr;

fn tool() -> ShellTool {
    tool_with_background(false).0
}

/// A shell tool plus the registry it was built against, so background tests
/// can observe and tear down what they start.
fn tool_with_background(allow_background: bool) -> (ShellTool, Arc<BackgroundTasks>) {
    let background = Arc::new(BackgroundTasks::temporary().unwrap());
    let tool = ShellTool::new(
        std::env::temp_dir().to_string_lossy().into_owned(),
        "coda".to_string(),
        allow_background.then(|| background.clone()),
    );
    (tool, background)
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

#[test]
fn description_describes_bash_and_the_fixed_timeout() {
    assert_eq!(
        tool().description(),
        "Execute Bash commands and return stdout and stderr. Commands have a fixed 2-minute timeout."
    );
}

#[tokio::test]
async fn completes_normally() {
    let out = tool()
        .execute(params("echo hello"), ToolCallContext::default())
        .await
        .unwrap();
    let OutputData::Inline(out) = out else {
        panic!("expected inline output")
    };
    assert_eq!(out, "hello\n");
}

#[tokio::test]
async fn normal_leader_exit_keeps_draining_its_child_output() {
    let output = tool()
        .execute(
            params("(sleep 0.7; printf child-output) & exit 0"),
            ToolCallContext::default(),
        )
        .await
        .unwrap();
    let output = output
        .materialize(
            &coda_core::output::BufferBudget::new(1024 * 1024),
            &coda_core::tool::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.text, "child-output");
}

#[tokio::test]
async fn reports_nonzero_exit() {
    let out = tool()
        .execute(params("echo oops >&2; exit 3"), ToolCallContext::default())
        .await
        .unwrap();
    let OutputData::Inline(out) = out else {
        panic!("expected inline output")
    };
    assert!(out.starts_with("exit code: 3"), "unexpected output: {out}");
    assert!(out.contains("oops"), "unexpected output: {out}");
}

#[tokio::test]
async fn timeout_kills_process_group() {
    let pidfile = std::env::temp_dir().join(format!("coda-shell-timeout-{}", std::process::id()));
    let _ = std::fs::remove_file(&pidfile);

    let mut shell = tool();
    shell.timeout = Duration::from_millis(500);
    let command = format!(
        "sleep 37.41 & echo \"$$ $!\" > '{}'; wait",
        pidfile.display()
    );
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        shell.execute(params(&command), ToolCallContext::default()),
    )
    .await
    .expect("shell timeout did not settle promptly");

    let reason = match result {
        Err(ToolError::ExecutionError(reason)) => reason,
        other => panic!("expected ExecutionError, got {other:?}"),
    };
    assert!(reason.contains("2-minute execution limit"));

    let pids: Vec<i32> = std::fs::read_to_string(&pidfile)
        .expect("command never wrote its pidfile")
        .split_whitespace()
        .map(|pid| pid.parse().expect("pidfile contained a non-PID"))
        .collect();
    assert_eq!(pids.len(), 2);
    let _cleanup: Vec<_> = pids.iter().copied().map(KillPidGuard).collect();

    tokio::time::timeout(Duration::from_secs(5), async {
        while pids.iter().any(|&pid| process_alive(pid)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("processes survived the shell timeout");

    let _ = std::fs::remove_file(&pidfile);
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
    let marker = std::env::temp_dir().join(format!("coda-shell-precancel-{}", std::process::id()));
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
    let marker = std::env::temp_dir().join(format!("coda-shell-sentinel-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);

    coda_execution::process::set_sentinel_failure(true);
    let result = tool()
        .execute(
            params(&format!("touch '{}'", marker.display())),
            ToolCallContext::default(),
        )
        .await;
    coda_execution::process::set_sentinel_failure(false);

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
    let pidfile = std::env::temp_dir().join(format!("coda-shell-linger-{}", std::process::id()));
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

/// The flag is a *capability*, granted with the follow-up tools: an agent
/// that cannot observe or kill a task must not be able to start one, and must
/// not even see the option.
#[test]
fn run_in_background_is_only_in_the_schema_once_granted() {
    let ungranted = tool_with_background(false).0;
    let props = ungranted.parameter_schema()["properties"]
        .as_object()
        .expect("schema has properties");
    assert!(props.contains_key("command"));
    assert!(!props.contains_key("run_in_background"));

    let granted = tool_with_background(true).0;
    assert!(
        granted.parameter_schema()["properties"]
            .as_object()
            .expect("schema has properties")
            .contains_key("run_in_background")
    );
    assert!(
        granted.description().contains("background task"),
        "a granted agent should be told what to do with long commands: {}",
        granted.description()
    );
}

#[tokio::test]
async fn an_ungranted_background_request_is_rejected_without_running_the_command() {
    let (shell, background) = tool_with_background(false);
    let marker =
        std::env::temp_dir().join(format!("coda-shell-no-background-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let error = shell
        .execute(
            background_params(&format!("touch '{}'", marker.display())),
            ToolCallContext::default(),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ToolError::ExecutionError(message) if message.contains("unavailable")));
    assert!(
        !marker.exists(),
        "the rejected command ran in the foreground"
    );
    assert!(
        background.summaries().borrow().is_empty(),
        "task was started"
    );
}

/// Backgrounding is how a command escapes the 2-minute limit, so the timeout
/// must not reach it: the call settles at once with an id, and the task is
/// still running well after a (shortened) timeout would have killed it.
#[tokio::test]
async fn a_background_task_settles_at_once_and_outlives_the_timeout() {
    let (mut shell, background) = tool_with_background(true);
    shell.timeout = Duration::from_millis(200);

    let out = tokio::time::timeout(
        Duration::from_secs(2),
        shell.execute(background_params("sleep 41.07"), ToolCallContext::default()),
    )
    .await
    .expect("background call did not settle promptly")
    .expect("background call failed");

    let OutputData::Inline(out) = out else {
        panic!("expected inline output")
    };
    let id = out
        .split_whitespace()
        .find(|word| word.starts_with("bg_"))
        .map(|word| word.trim_end_matches('.'))
        .expect("no task id in the result");
    let id = coda_execution::TaskId::from_str(id).expect("well-formed task id");

    tokio::time::sleep(Duration::from_millis(600)).await;
    let read = background
        .read_result(&id)
        .await
        .expect("registry readable")
        .expect("task still known");
    assert!(
        matches!(read, coda_execution::TaskResult::Pending { .. }),
        "the foreground timeout reached a background task"
    );

    background.shutdown().await;
}

fn context_with_store(store: Arc<coda_output::Store>) -> ToolCallContext {
    let mut context = ToolCallContext::default();
    context.outputs = Some(coda_core::output::OutputRuntime {
        store,
        owner: Default::default(),
        ptc: Default::default(),
    });
    context
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_large_command_keeps_its_middle_log_readable() {
    use coda_core::output::{Channel, OutputData, OutputLimits, OutputStore};
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(
        coda_output::Store::open(OutputLimits {
            root: root.path().join("output"),
            capture_memory_bytes: 65536,
            ..OutputLimits::default()
        })
        .unwrap(),
    );
    let ready = root.path().join("ready");
    let script = format!(
        "head -c 131072 /dev/zero | tr '\\0' A; printf 'MIDDLE-BEFORE-CANCEL'; head -c 131072 /dev/zero | tr '\\0' Z; touch '{}'; sleep 30",
        ready.display()
    );
    let context = context_with_store(store.clone());
    let saved = context.clone();
    let task = tokio::spawn(tool().execute(params(&script), context));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let start = tokio::time::Instant::now();
    saved.cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, ToolError::Aborted(_)));
    assert!(start.elapsed() < Duration::from_secs(3));
    let OutputData::Captured(output) = saved.take_failure_output().unwrap() else {
        panic!("lost captured output")
    };
    let reference = output.reference.unwrap();
    let file = reference
        .channels
        .iter()
        .find(|c| c.channel == Channel::Stdout)
        .unwrap();
    assert!(
        std::fs::read_to_string(&file.path)
            .unwrap()
            .contains("MIDDLE-BEFORE-CANCEL")
    );
    assert!(
        !output
            .preview
            .render(16 * 1024)
            .0
            .contains("MIDDLE-BEFORE-CANCEL")
    );
    assert!(store.charged_bytes() <= store.limits().total_disk_bytes);
}

#[tokio::test]
async fn exceeding_disk_quota_does_not_stop_the_command_or_lose_its_exit_status() {
    use coda_core::output::{OutputData, OutputLimits, StorageFailure};
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(
        coda_output::Store::open(OutputLimits {
            root: root.path().join("output"),
            capture_memory_bytes: 65536,
            result_max_bytes: 1024 * 1024,
            ..OutputLimits::default()
        })
        .unwrap(),
    );
    let context = context_with_store(store.clone());
    let output = tool()
        .execute(
            params("head -c 33554432 /dev/zero; printf '\\nEXIT-MARKER'"),
            context,
        )
        .await
        .unwrap();
    let OutputData::Captured(output) = output else {
        panic!()
    };
    assert_eq!(output.failure, Some(StorageFailure::ResultLimit));
    assert!(
        output
            .preview
            .render(16 * 1024)
            .0
            .ends_with("\nEXIT-MARKER")
    );
    let reference = output.reference.unwrap();
    assert!(!reference.complete);
    assert_eq!(
        reference
            .channels
            .iter()
            .map(|c| c.saved_bytes)
            .sum::<u64>(),
        1024 * 1024
    );
    assert!(store.charged_bytes() < 2 * 1024 * 1024);
}
