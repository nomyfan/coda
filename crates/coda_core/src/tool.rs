use std::boxed::Box;
use std::collections::BTreeMap;
use std::fmt::{Debug, Display};
use std::pin::Pin;
use std::sync::Arc;

use serde::de::DeserializeOwned;
pub use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, info, info_span};

use super::llm::{ToolArtifact, ToolDefinition};

/// A tool's durable state on the calling thread, keyed by an opaque `kind`.
///
/// The runtime stores what a tool puts here **anchored to the message that
/// records the call** — so state is cut by exactly the rule that cuts the
/// conversation. A rewind that drops a turn drops the state written in it; a
/// fork keeps whatever the kept turns wrote. Neither operation, and nothing
/// else in the runtime, needs to know what any `kind` means.
///
/// Each `set` records a *complete* value, not a delta. That is what lets the
/// runtime collapse a range of entries — which is what a compaction does — by
/// keeping the last one of each kind, without a per-kind reducer.
///
/// Reads see the thread as it stood when this batch of calls was dispatched,
/// plus anything this same call has already `set`. A batch runs concurrently
/// and has no order to observe, so sibling calls never see each other land.
pub trait ThreadState: Send + Sync {
    fn get(&self, kind: &str) -> Option<serde_json::Value>;
    fn set(&self, kind: &str, value: serde_json::Value);
}

/// The state handed to a tool with no thread behind it — a standalone build, a
/// test. Reads empty and discards writes, so tools need no `Option` for a case
/// that only means "nothing is recording".
pub struct NoState;

impl ThreadState for NoState {
    fn get(&self, _kind: &str) -> Option<serde_json::Value> {
        None
    }
    fn set(&self, _kind: &str, _value: serde_json::Value) {}
}

/// Per-invocation execution context handed to every tool call.
///
/// `cancel` fires when the caller aborts the invocation (e.g. the user aborts
/// the turn). Tools driving external work — child processes, network calls —
/// should observe it, tear that work down, and return [`ToolError::Aborted`]
/// promptly; quick in-process tools may ignore it and run to completion.
#[derive(Clone)]
pub struct ToolCallContext {
    pub cancel: CancellationToken,
    /// Where a tool keeps anything that has to outlive the call — see
    /// [`ThreadState`].
    pub state: Arc<dyn ThreadState>,
    artifacts: Arc<std::sync::Mutex<Vec<ToolArtifact>>>,
}

impl ToolCallContext {
    pub fn new(cancel: CancellationToken, state: Arc<dyn ThreadState>) -> Self {
        Self {
            cancel,
            state,
            artifacts: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Attach immutable presentation data to this call's eventual tool message.
    pub fn record_artifact(&self, artifact: ToolArtifact) {
        self.artifacts.lock().unwrap().push(artifact);
    }

    /// Drain artifacts after execution so they are persisted with the result.
    pub fn take_artifacts(&self) -> Vec<ToolArtifact> {
        std::mem::take(&mut *self.artifacts.lock().unwrap())
    }
}

impl Default for ToolCallContext {
    fn default() -> Self {
        ToolCallContext::new(CancellationToken::new(), Arc::new(NoState))
    }
}

impl Debug for ToolCallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallContext")
            .field("cancel", &self.cancel)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum ToolError {
    InvalidParameters(String),
    ExecutionError(String),
    /// The call observed cancellation and stopped early. The payload becomes
    /// the recorded tool result and may carry partial output.
    Aborted(String),
}

impl Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::InvalidParameters(reason) => write!(f, "Invalid parameters: {}", reason),
            ToolError::ExecutionError(reason) => write!(f, "Execution error: {}", reason),
            ToolError::Aborted(reason) => write!(f, "Aborted: {}", reason),
        }
    }
}

pub type ToolResult<T> = Result<T, ToolError>;

impl<T> From<ToolError> for ToolResult<T> {
    fn from(value: ToolError) -> Self {
        Err(value)
    }
}

pub trait Tool: Send + Sync + 'static {
    type Parameters: DeserializeOwned + Send;
    type Output: Display + Send;

    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameter_schema(&self) -> &serde_json::Value;
    fn execute(
        &self,
        params: Self::Parameters,
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static;
}

pub trait ToolObject: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameter_schema(&self) -> &serde_json::Value;
    fn execute(
        self: Arc<Self>,
        params: String,
        ctx: ToolCallContext,
    ) -> Pin<Box<dyn Future<Output = ToolResult<String>> + Send>>;
}

pub struct ToolWrapper<T: Tool>(T);

impl<T: Tool> ToolObject for ToolWrapper<T> {
    #[inline]
    fn name(&self) -> &str {
        self.0.name()
    }

    #[inline]
    fn description(&self) -> &str {
        self.0.description()
    }

    #[inline]
    fn parameter_schema(&self) -> &serde_json::Value {
        self.0.parameter_schema()
    }

    fn execute(
        self: Arc<Self>,
        input: String,
        ctx: ToolCallContext,
    ) -> Pin<Box<dyn Future<Output = ToolResult<String>> + Send>> {
        let span = info_span!(
            "execute_tool",
            tool = self.name(),
            input = &input,
            output = tracing::field::Empty,
            error = tracing::field::Empty
        );
        let params: T::Parameters = match serde_json::from_str(&input) {
            Ok(input) => input,
            Err(err) => {
                let reason = format!("{}", err);
                return Box::pin(async move { ToolError::InvalidParameters(reason).into() });
            }
        };

        Box::pin(
            async move {
                info!("executing tool");
                let result = self.0.execute(params, ctx).await;
                let span = Span::current();
                match &result {
                    Ok(output) => span.record("output", output.to_string()),
                    Err(err) => span.record("error", err.to_string()),
                };
                result.map(|output| output.to_string())
            }
            .instrument(span),
        )
    }
}

impl<T: Tool> From<T> for ToolWrapper<T> {
    fn from(value: T) -> Self {
        ToolWrapper(value)
    }
}

#[derive(Clone, Default)]
pub struct Tools(BTreeMap<String, Arc<dyn ToolObject>>);

impl Tools {
    pub fn register(&mut self, tool: Box<dyn ToolObject>) {
        self.0.insert(tool.name().to_string(), Arc::from(tool));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolObject>> {
        self.0.get(name).cloned()
    }

    pub fn descriptors(&self) -> Vec<ToolDefinition> {
        self.0
            .values()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameter_schema: tool.parameter_schema().clone(),
            })
            .collect()
    }
}
