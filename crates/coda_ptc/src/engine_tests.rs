use super::*;

use std::pin::Pin;

use coda_core::llm::{FileChangeOperation, ToolArtifact};
use coda_core::tool::{
    HostEffectError, HostEffectLimits, HostToolCallResult, ThreadState, ToolCallContext,
};

struct FakeInvoker {
    exposed: Arc<[String]>,
    delay: Duration,
}

impl FakeInvoker {
    fn new(names: &[&str]) -> Self {
        Self {
            exposed: Arc::from(
                names
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect::<Vec<_>>(),
            ),
            delay: Duration::ZERO,
        }
    }
}

impl HostToolInvoker for FakeInvoker {
    fn exposed_tools(&self) -> Arc<[String]> {
        self.exposed.clone()
    }

    fn call(
        &self,
        name: String,
        arguments: String,
        _context: ToolCallContext,
    ) -> Pin<Box<dyn Future<Output = Result<HostToolCallResult, HostToolCallError>> + Send>> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(HostToolCallResult {
                output: format!("{name}:{arguments}"),
            })
        })
    }
}

struct FailingInvoker;

impl HostToolInvoker for FailingInvoker {
    fn exposed_tools(&self) -> Arc<[String]> {
        Arc::from(vec!["read_file".to_string()])
    }

    fn call(
        &self,
        _name: String,
        _arguments: String,
        _context: ToolCallContext,
    ) -> Pin<Box<dyn Future<Output = Result<HostToolCallResult, HostToolCallError>> + Send>> {
        Box::pin(async {
            Err(HostToolCallError::Execution(
                "Failed to open file: No such file or directory".to_string(),
            ))
        })
    }
}

#[derive(Default)]
struct RecordingState(std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>);

impl ThreadState for RecordingState {
    fn get(&self, kind: &str) -> Option<serde_json::Value> {
        self.0.lock().unwrap().get(kind).cloned()
    }

    fn set(&self, kind: &str, value: serde_json::Value) -> Result<(), HostEffectError> {
        self.0.lock().unwrap().insert(kind.to_string(), value);
        Ok(())
    }
}

struct EffectInvoker;

impl HostToolInvoker for EffectInvoker {
    fn exposed_tools(&self) -> Arc<[String]> {
        Arc::from(vec!["write_file".to_string()])
    }

    fn call(
        &self,
        _name: String,
        _arguments: String,
        context: ToolCallContext,
    ) -> Pin<Box<dyn Future<Output = Result<HostToolCallResult, HostToolCallError>> + Send>> {
        Box::pin(async move {
            context
                .state
                .set("effect", serde_json::json!("staged"))
                .unwrap();
            context
                .record_artifact(ToolArtifact::FileDiff {
                    path: "file.txt".to_string(),
                    operation: FileChangeOperation::Create,
                    patch: "patch".to_string(),
                })
                .unwrap();
            Ok(HostToolCallResult {
                output: "oversized".to_string(),
            })
        })
    }
}

fn scope() -> NestedCallScope {
    NestedCallScope::new(
        ToolCallContext::default(),
        HostEffectLimits {
            state_bytes: 1024 * 1024,
            artifact_bytes: 1024 * 1024,
        },
    )
}

async fn run(code: &str, names: &[&str], limits: PtcLimits) -> JsRunReport {
    let invoker = Arc::new(FakeInvoker::new(names));
    JsExecutor::new(limits)
        .run(
            code.to_string(),
            invoker.exposed_tools(),
            invoker,
            scope(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

async fn run_with_tool_error(code: &str) -> JsRunReport {
    let invoker = Arc::new(FailingInvoker);
    JsExecutor::new(PtcLimits::default())
        .run(
            code.to_string(),
            invoker.exposed_tools(),
            invoker,
            scope(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn returns_json_value_and_bounded_console_output() {
    let report = run(
        r#"
console.log("hello", { answer: 42 });
return { answer: 42 };
"#,
        &["read_file"],
        PtcLimits::default(),
    )
    .await;

    assert!(report.ok);
    assert_eq!(report.value, Some(serde_json::json!({ "answer": 42 })));
    assert_eq!(report.stdout, "hello {\"answer\":42}");
    assert!(!report.stdout_truncated);
}

#[tokio::test]
async fn awaits_host_calls_and_hides_unexposed_tools() {
    let report = run(
        r#"
const calls = await Promise.all([
  tools.read_file({ file_path: "a" }),
  tools.read_file({ file_path: "b" }),
]);
return {
  calls,
  hidden: typeof tools.write_file,
  bridge: typeof globalThis.__coda_call_tool,
  consoleInfo: typeof console.info,
  frozen: Object.isFrozen(tools),
};
"#,
        &["read_file"],
        PtcLimits::default(),
    )
    .await;

    assert!(report.ok, "{report:?}");
    assert_eq!(report.completed_calls, 2);
    assert_eq!(report.value.as_ref().unwrap()["hidden"], "undefined");
    assert_eq!(report.value.as_ref().unwrap()["bridge"], "undefined");
    assert_eq!(report.value.as_ref().unwrap()["consoleInfo"], "undefined");
    assert_eq!(report.value.as_ref().unwrap()["frozen"], true);
    assert_eq!(
        report.value.as_ref().unwrap()["calls"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn syntax_and_runtime_exceptions_are_structured_reports() {
    let syntax = run("return (", &["read_file"], PtcLimits::default()).await;
    assert_eq!(syntax.error.unwrap().code, "SYNTAX_ERROR");

    let thrown = run(
        "throw new Error('boom');",
        &["read_file"],
        PtcLimits::default(),
    )
    .await;
    assert_eq!(thrown.error.as_ref().unwrap().code, "JS_EXCEPTION");
    assert_eq!(thrown.error.unwrap().message, "boom");
}

#[tokio::test]
async fn uncaught_tool_error_preserves_its_code() {
    let report = run_with_tool_error("await tools.read_file({ file_path: 'missing' });").await;

    assert!(!report.ok);
    assert_eq!(report.completed_calls, 1);
    assert_eq!(report.error.as_ref().unwrap().code, "TOOL_ERROR");
    assert_eq!(
        report.error.unwrap().message,
        "Failed to open file: No such file or directory"
    );
}

#[tokio::test]
async fn caught_tool_error_is_typed_and_serializable() {
    let report = run_with_tool_error(
        r#"
try {
  await tools.read_file({ file_path: "missing" });
} catch (error) {
  return {
    type: typeof error,
    isError: error instanceof Error,
    name: error.name,
    code: error.code,
    message: error.message,
    keys: Object.keys(error),
    serialized: JSON.stringify(error),
  };
}
"#,
    )
    .await;

    assert!(report.ok, "{report:?}");
    assert_eq!(report.completed_calls, 1);
    let value = report.value.unwrap();
    assert_eq!(value["type"], "object");
    assert_eq!(value["isError"], true);
    assert_eq!(value["name"], "ToolError");
    assert_eq!(value["code"], "TOOL_ERROR");
    assert_eq!(
        value["message"],
        "Failed to open file: No such file or directory"
    );
    let keys = value["keys"].as_array().unwrap();
    assert!(keys.iter().any(|key| key == "message"));
    assert!(keys.iter().any(|key| key == "name"));
    assert!(keys.iter().any(|key| key == "code"));
    let serialized: serde_json::Value =
        serde_json::from_str(value["serialized"].as_str().unwrap()).unwrap();
    assert_eq!(serialized["name"], "ToolError");
    assert_eq!(serialized["code"], "TOOL_ERROR");
    assert_eq!(
        serialized["message"],
        "Failed to open file: No such file or directory"
    );
}

#[tokio::test]
async fn all_settled_keeps_the_tool_error_details() {
    let report = run_with_tool_error(
        r#"
const [settled] = await Promise.allSettled([
  tools.read_file({ file_path: "missing" }),
]);
return settled;
"#,
    )
    .await;

    assert!(report.ok, "{report:?}");
    assert_eq!(report.completed_calls, 1);
    let value = report.value.unwrap();
    assert_eq!(value["status"], "rejected");
    assert_eq!(value["reason"]["name"], "ToolError");
    assert_eq!(value["reason"]["code"], "TOOL_ERROR");
    assert_eq!(
        value["reason"]["message"],
        "Failed to open file: No such file or directory"
    );
}

#[tokio::test]
async fn nested_call_and_result_limits_reject_inside_javascript() {
    let call_limits = PtcLimits {
        max_calls: 1,
        ..PtcLimits::default()
    };
    let call_report = run(
        "await tools.read_file({}); return await tools.read_file({});",
        &["read_file"],
        call_limits,
    )
    .await;
    assert_eq!(call_report.error.unwrap().code, "CALL_LIMIT");

    let result_limits = PtcLimits {
        result_bytes: 4,
        ..PtcLimits::default()
    };
    let result_report = run(
        "return await tools.read_file({ file_path: 'long' });",
        &["read_file"],
        result_limits,
    )
    .await;
    assert_eq!(result_report.error.unwrap().code, "RESULT_LIMIT");
}

#[tokio::test]
async fn result_limit_discards_the_childs_staged_effects() {
    let limits = PtcLimits {
        result_bytes: 4,
        ..PtcLimits::default()
    };
    let state = Arc::new(RecordingState::default());
    let outer = ToolCallContext::new(CancellationToken::new(), state.clone());
    let inspect_artifacts = outer.clone();
    let scope = NestedCallScope::new(
        outer,
        HostEffectLimits {
            state_bytes: limits.state_bytes,
            artifact_bytes: limits.artifact_bytes,
        },
    );
    let commit = scope.clone();
    let invoker = Arc::new(EffectInvoker);
    let report = JsExecutor::new(limits)
        .run(
            "return await tools.write_file({});".to_string(),
            invoker.exposed_tools(),
            invoker,
            scope,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(report.error.unwrap().code, "RESULT_LIMIT");
    commit.commit_into_outer().unwrap();
    assert_eq!(state.get("effect"), None);
    assert!(inspect_artifacts.take_artifacts().is_empty());
}

#[tokio::test]
async fn unfinished_fire_and_forget_call_is_reported_and_cancelled() {
    let invoker = Arc::new(FakeInvoker {
        exposed: Arc::from(vec!["write_file".to_string()]),
        delay: Duration::from_secs(10),
    });
    let report = JsExecutor::new(PtcLimits::default())
        .run(
            "tools.write_file({}); return 'done';".to_string(),
            invoker.exposed_tools(),
            invoker,
            scope(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(!report.ok);
    assert_eq!(report.error.unwrap().code, "UNAWAITED_TOOL_CALLS");
}

#[tokio::test]
async fn worker_queue_wait_observes_the_wall_clock_deadline() {
    let workers = Arc::new(tokio::sync::Semaphore::new(1));
    let _held = workers.clone().acquire_owned().await.unwrap();
    let limits = PtcLimits {
        wall_time: Duration::from_millis(30),
        ..PtcLimits::default()
    };
    let executor = JsExecutor { limits, workers };
    let report = executor
        .run(
            "return null;".to_string(),
            Arc::from(vec!["read_file".to_string()]),
            Arc::new(FakeInvoker::new(&["read_file"])),
            scope(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(report.error.unwrap().code, "DEADLINE_EXCEEDED");
}

#[tokio::test]
async fn worker_queue_wait_observes_cancellation() {
    let workers = Arc::new(tokio::sync::Semaphore::new(1));
    let _held = workers.clone().acquire_owned().await.unwrap();
    let executor = JsExecutor {
        limits: PtcLimits::default(),
        workers,
    };
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = executor
        .run(
            "return null;".to_string(),
            Arc::from(vec!["read_file".to_string()]),
            Arc::new(FakeInvoker::new(&["read_file"])),
            scope(),
            cancel,
        )
        .await;

    assert!(matches!(result, Err(JsEngineError::Aborted(_))));
}

#[tokio::test]
async fn external_watchdog_interrupts_cpu_loop() {
    let limits = PtcLimits {
        wall_time: Duration::from_millis(50),
        join_grace: Duration::from_secs(2),
        ..PtcLimits::default()
    };
    let report = run("while (true) {}", &["read_file"], limits).await;

    assert!(!report.ok);
    assert_eq!(report.error.unwrap().code, "DEADLINE_EXCEEDED");
}

#[tokio::test]
async fn deadline_wakes_a_permanently_pending_promise() {
    let limits = PtcLimits {
        wall_time: Duration::from_millis(40),
        ..PtcLimits::default()
    };
    let report = run("await new Promise(() => {});", &["read_file"], limits).await;

    assert_eq!(report.error.unwrap().code, "DEADLINE_EXCEEDED");
}

#[tokio::test]
async fn deadline_cancels_a_pending_host_call() {
    let limits = PtcLimits {
        wall_time: Duration::from_millis(40),
        ..PtcLimits::default()
    };
    let invoker = Arc::new(FakeInvoker {
        exposed: Arc::from(vec!["read_file".to_string()]),
        delay: Duration::from_secs(10),
    });
    let report = JsExecutor::new(limits)
        .run(
            "return await tools.read_file({ file_path: 'a' });".to_string(),
            invoker.exposed_tools(),
            invoker,
            scope(),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(report.error.unwrap().code, "DEADLINE_EXCEEDED");
}

#[tokio::test]
async fn cancellation_wakes_a_permanently_pending_promise() {
    let invoker = Arc::new(FakeInvoker::new(&["read_file"]));
    let cancel = CancellationToken::new();
    let cancel_from_test = cancel.clone();
    let task = tokio::spawn(async move {
        JsExecutor::new(PtcLimits::default())
            .run(
                "await new Promise(() => {});".to_string(),
                invoker.exposed_tools(),
                invoker,
                scope(),
                cancel,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    cancel_from_test.cancel();

    assert!(matches!(
        task.await.unwrap(),
        Err(JsEngineError::Aborted(_))
    ));
}

#[tokio::test]
async fn console_overflow_keeps_the_tail() {
    let limits = PtcLimits {
        stdout_bytes: 8,
        ..PtcLimits::default()
    };
    let report = run(
        "console.log('first'); console.log('second'); return null;",
        &["read_file"],
        limits,
    )
    .await;

    assert_eq!(report.stdout, "second");
    assert!(report.stdout_truncated);
}

#[tokio::test]
async fn oversized_final_value_becomes_a_reported_error() {
    let limits = PtcLimits {
        final_bytes: 32,
        ..PtcLimits::default()
    };
    let report = run("return 'x'.repeat(100);", &["read_file"], limits).await;

    assert_eq!(report.error.unwrap().code, "OUTPUT_LIMIT");
}
