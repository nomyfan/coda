use super::*;

use std::collections::HashMap;
use std::io::Write;

use crate::llm::FileChangeOperation;

#[derive(Default)]
struct RecordingState {
    values: std::sync::Mutex<HashMap<String, serde_json::Value>>,
    writes: std::sync::Mutex<Vec<String>>,
}

impl ThreadState for RecordingState {
    fn get(&self, kind: &str) -> Option<serde_json::Value> {
        self.values.lock().unwrap().get(kind).cloned()
    }

    fn set(&self, kind: &str, value: serde_json::Value) -> Result<(), HostEffectError> {
        self.values.lock().unwrap().insert(kind.to_string(), value);
        self.writes.lock().unwrap().push(kind.to_string());
        Ok(())
    }
}

fn limits() -> HostEffectLimits {
    HostEffectLimits {
        state_bytes: 1024,
        artifact_bytes: 1024,
    }
}

#[test]
fn staged_state_is_last_write_wins_and_commits_outer_once() {
    let state = Arc::new(RecordingState::default());
    let outer = ToolCallContext::new(CancellationToken::new(), state.clone());
    let scope = HostCallScope::new(outer, limits());

    let first = scope.begin_tool_call(CancellationToken::new());
    first
        .context()
        .state
        .set("todos", serde_json::json!([1]))
        .unwrap();
    first
        .context()
        .state
        .set("todos", serde_json::json!([2]))
        .unwrap();
    first.commit();

    assert_eq!(state.get("todos"), None);
    let second = scope.begin_tool_call(CancellationToken::new());
    assert_eq!(
        second.context().state.get("todos"),
        Some(serde_json::json!([2]))
    );
    second
        .context()
        .state
        .set("todos", serde_json::json!([3]))
        .unwrap();
    second.commit();

    let duplicate_finalizer = scope.clone();
    scope.commit_into_outer().unwrap();
    duplicate_finalizer.commit_into_outer().unwrap();
    assert_eq!(state.get("todos"), Some(serde_json::json!([3])));
    assert_eq!(&*state.writes.lock().unwrap(), &["todos"]);
}

#[test]
fn dropped_child_releases_budget_and_does_not_commit() {
    let state = Arc::new(RecordingState::default());
    let outer = ToolCallContext::new(CancellationToken::new(), state.clone());
    let scope = HostCallScope::new(
        outer,
        HostEffectLimits {
            state_bytes: 6,
            artifact_bytes: 4,
        },
    );

    let discarded = scope.begin_tool_call(CancellationToken::new());
    discarded
        .context()
        .state
        .set("key", serde_json::json!("a"))
        .unwrap();
    drop(discarded);

    let kept = scope.begin_tool_call(CancellationToken::new());
    kept.context()
        .state
        .set("key", serde_json::json!("b"))
        .unwrap();
    kept.commit();
    scope.commit_into_outer().unwrap();
    assert_eq!(state.get("key"), Some(serde_json::json!("b")));
}

#[test]
fn artifact_budget_is_checked_before_retention() {
    let outer = ToolCallContext::default();
    let scope = HostCallScope::new(
        outer,
        HostEffectLimits {
            state_bytes: 1024,
            artifact_bytes: 3,
        },
    );
    let child = scope.begin_tool_call(CancellationToken::new());
    let error = child
        .context()
        .record_artifact(ToolArtifact::FileDiff {
            path: "a".to_string(),
            operation: FileChangeOperation::Create,
            patch: "long".to_string(),
        })
        .unwrap_err();
    assert_eq!(error.resource, "host tool artifacts");
}

#[derive(Clone)]
struct CaptureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

struct CaptureGuard(Arc<std::sync::Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureGuard;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureGuard(self.0.clone())
    }
}

impl Write for CaptureGuard {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct SecretTool {
    schema: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct SecretParams {
    secret: String,
}

impl Tool for SecretTool {
    type Parameters = SecretParams;
    type Output = String;

    fn name(&self) -> &str {
        "secret"
    }

    fn description(&self) -> &str {
        "test"
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        &self.schema
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
        _ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        async move { Ok(format!("returned:{}", params.secret)) }
    }
}

#[test]
fn tool_tracing_records_sizes_but_not_raw_input_or_output() {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(CaptureWriter(captured.clone()))
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let sentinel = "SENTINEL_private_file_contents";
    let tool: Arc<dyn ToolObject> = Arc::new(ToolWrapper::from(SecretTool {
        schema: serde_json::json!({ "type": "object" }),
    }));
    let result = futures::executor::block_on(tool.execute(
        serde_json::json!({ "secret": sentinel }).to_string(),
        ToolCallContext::default(),
    ));
    assert!(result.is_ok());

    let logs = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert!(!logs.contains(sentinel), "{logs}");
    assert!(logs.contains("input_bytes"), "{logs}");
    assert!(logs.contains("output_bytes"), "{logs}");
    assert!(logs.contains("status"), "{logs}");
}
