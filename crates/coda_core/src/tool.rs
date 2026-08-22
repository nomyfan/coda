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
    fn set(&self, kind: &str, value: serde_json::Value) -> Result<(), HostEffectError>;
}

/// The state handed to a tool with no thread behind it — a standalone build, a
/// test. Reads empty and discards writes, so tools need no `Option` for a case
/// that only means "nothing is recording".
pub struct NoState;

impl ThreadState for NoState {
    fn get(&self, _kind: &str) -> Option<serde_json::Value> {
        None
    }
    fn set(&self, _kind: &str, _value: serde_json::Value) -> Result<(), HostEffectError> {
        Ok(())
    }
}

/// A retained host-side effect would exceed the budget assigned to this call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEffectError {
    pub resource: &'static str,
    pub limit_bytes: usize,
}

impl Display for HostEffectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} exceeds the {} byte host-effect budget",
            self.resource, self.limit_bytes
        )
    }
}

impl std::error::Error for HostEffectError {}

/// Result returned by a tool invoked through a programmatic host bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostToolCallResult {
    pub output: String,
}

/// Provider-independent failures at the host-call trust boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostToolCallError {
    Unavailable,
    InvalidParameters(String),
    Execution(String),
    ResourceLimit(String),
    Aborted(String),
}

/// The only capability that lets a tool call another registered tool.
///
/// Normal [`ToolCallContext`] values do not carry one. The agent integration
/// installs it only for `run_javascript`, and each nested call receives a child
/// context with the invoker removed.
pub trait HostToolInvoker: Send + Sync {
    /// Frozen generation-time capability snapshot, in descriptor order.
    fn exposed_tools(&self) -> Arc<[String]>;

    fn call(
        &self,
        name: String,
        arguments: String,
        context: ToolCallContext,
    ) -> Pin<Box<dyn Future<Output = Result<HostToolCallResult, HostToolCallError>> + Send>>;
}

trait ArtifactSink: Send + Sync {
    fn record(&self, artifact: ToolArtifact) -> Result<(), HostEffectError>;
    fn take(&self) -> Vec<ToolArtifact>;
}

#[derive(Default)]
struct UnboundedArtifactSink(std::sync::Mutex<Vec<ToolArtifact>>);

impl ArtifactSink for UnboundedArtifactSink {
    fn record(&self, artifact: ToolArtifact) -> Result<(), HostEffectError> {
        self.0.lock().unwrap().push(artifact);
        Ok(())
    }

    fn take(&self) -> Vec<ToolArtifact> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
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
    artifacts: Arc<dyn ArtifactSink>,
    invoker: Option<Arc<dyn HostToolInvoker>>,
}

impl ToolCallContext {
    pub fn new(cancel: CancellationToken, state: Arc<dyn ThreadState>) -> Self {
        Self {
            cancel,
            state,
            artifacts: Arc::new(UnboundedArtifactSink::default()),
            invoker: None,
        }
    }

    /// Install the narrowly scoped host-call capability for a runner tool.
    pub fn with_invoker(mut self, invoker: Arc<dyn HostToolInvoker>) -> Self {
        self.invoker = Some(invoker);
        self
    }

    pub fn invoker(&self) -> Option<Arc<dyn HostToolInvoker>> {
        self.invoker.clone()
    }

    /// Attach immutable presentation data to this call's eventual tool message.
    pub fn record_artifact(&self, artifact: ToolArtifact) -> Result<(), HostEffectError> {
        self.artifacts.record(artifact)
    }

    /// Drain artifacts after execution so they are persisted with the result.
    pub fn take_artifacts(&self) -> Vec<ToolArtifact> {
        self.artifacts.take()
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
    ResourceLimit(String),
    /// The call observed cancellation and stopped early. The payload becomes
    /// the recorded tool result and may carry partial output.
    Aborted(String),
}

impl Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::InvalidParameters(reason) => write!(f, "Invalid parameters: {}", reason),
            ToolError::ExecutionError(reason) => write!(f, "Execution error: {}", reason),
            ToolError::ResourceLimit(reason) => write!(f, "Resource limit: {}", reason),
            ToolError::Aborted(reason) => write!(f, "Aborted: {}", reason),
        }
    }
}

impl From<HostEffectError> for ToolError {
    fn from(value: HostEffectError) -> Self {
        ToolError::ResourceLimit(value.to_string())
    }
}

/// Limits for effects retained by one programmatic script.
#[derive(Debug, Clone, Copy)]
pub struct HostEffectLimits {
    pub state_bytes: usize,
    pub artifact_bytes: usize,
}

struct ScopedEffects {
    state: BTreeMap<String, (serde_json::Value, usize)>,
    artifacts: Vec<ToolArtifact>,
    reserved_state_bytes: usize,
    reserved_artifact_bytes: usize,
    finalized: bool,
}

struct NestedScopeInner {
    outer: ToolCallContext,
    limits: HostEffectLimits,
    effects: std::sync::Mutex<ScopedEffects>,
}

/// Script-wide staging area for nested tool effects.
#[derive(Clone)]
pub struct NestedCallScope(Arc<NestedScopeInner>);

impl NestedCallScope {
    pub fn new(outer: ToolCallContext, limits: HostEffectLimits) -> Self {
        Self(Arc::new(NestedScopeInner {
            outer,
            limits,
            effects: std::sync::Mutex::new(ScopedEffects {
                state: BTreeMap::new(),
                artifacts: Vec::new(),
                reserved_state_bytes: 0,
                reserved_artifact_bytes: 0,
                finalized: false,
            }),
        }))
    }

    /// Create an isolated child call. Its context deliberately has no invoker.
    pub fn for_nested_call(&self, cancel: CancellationToken) -> NestedToolCall {
        let state = Arc::new(NestedThreadState {
            scope: self.0.clone(),
            writes: std::sync::Mutex::new(BTreeMap::new()),
        });
        let artifacts = Arc::new(NestedArtifactSink {
            scope: self.0.clone(),
            artifacts: std::sync::Mutex::new(Vec::new()),
        });
        let context = ToolCallContext {
            cancel,
            state: state.clone(),
            artifacts: artifacts.clone(),
            invoker: None,
        };
        NestedToolCall {
            scope: self.clone(),
            state,
            artifacts,
            context,
            committed: false,
        }
    }

    /// Consume the scope and write each final state key once to the outer call.
    pub fn commit_into_outer(self) -> Result<(), HostEffectError> {
        let (state, artifacts) = {
            let mut effects = self.0.effects.lock().unwrap();
            if effects.finalized {
                return Ok(());
            }
            effects.finalized = true;
            let state = std::mem::take(&mut effects.state);
            let artifacts = std::mem::take(&mut effects.artifacts);
            effects.reserved_state_bytes = 0;
            effects.reserved_artifact_bytes = 0;
            (state, artifacts)
        };
        for (kind, (value, _)) in state {
            self.0.outer.state.set(&kind, value)?;
        }
        for artifact in artifacts {
            self.0.outer.record_artifact(artifact)?;
        }
        Ok(())
    }
}

struct NestedThreadState {
    scope: Arc<NestedScopeInner>,
    writes: std::sync::Mutex<BTreeMap<String, (serde_json::Value, usize)>>,
}

impl NestedThreadState {
    fn discard(&self) {
        let mut writes = self.writes.lock().unwrap();
        let released = writes.values().map(|(_, bytes)| *bytes).sum::<usize>();
        writes.clear();
        let mut effects = self.scope.effects.lock().unwrap();
        effects.reserved_state_bytes = effects.reserved_state_bytes.saturating_sub(released);
    }
}

impl ThreadState for NestedThreadState {
    fn get(&self, kind: &str) -> Option<serde_json::Value> {
        if let Some((value, _)) = self.writes.lock().unwrap().get(kind) {
            return Some(value.clone());
        }
        if let Some((value, _)) = self.scope.effects.lock().unwrap().state.get(kind) {
            return Some(value.clone());
        }
        self.scope.outer.state.get(kind)
    }

    fn set(&self, kind: &str, value: serde_json::Value) -> Result<(), HostEffectError> {
        let bytes = kind
            .len()
            .saturating_add(serde_json::to_vec(&value).map_or(usize::MAX, |encoded| encoded.len()));
        let mut writes = self.writes.lock().unwrap();
        let old_bytes = writes.get(kind).map_or(0, |(_, bytes)| *bytes);
        let mut effects = self.scope.effects.lock().unwrap();
        if effects.finalized {
            return Err(HostEffectError {
                resource: "finalized nested call scope",
                limit_bytes: 0,
            });
        }
        let next = effects
            .reserved_state_bytes
            .saturating_sub(old_bytes)
            .saturating_add(bytes);
        if next > self.scope.limits.state_bytes {
            return Err(HostEffectError {
                resource: "nested tool state",
                limit_bytes: self.scope.limits.state_bytes,
            });
        }
        effects.reserved_state_bytes = next;
        writes.insert(kind.to_string(), (value, bytes));
        Ok(())
    }
}

struct NestedArtifactSink {
    scope: Arc<NestedScopeInner>,
    artifacts: std::sync::Mutex<Vec<(ToolArtifact, usize)>>,
}

impl NestedArtifactSink {
    fn discard(&self) {
        let mut artifacts = self.artifacts.lock().unwrap();
        let released = artifacts.iter().map(|(_, bytes)| *bytes).sum::<usize>();
        artifacts.clear();
        let mut effects = self.scope.effects.lock().unwrap();
        effects.reserved_artifact_bytes = effects.reserved_artifact_bytes.saturating_sub(released);
    }

    fn take_sized(&self) -> Vec<(ToolArtifact, usize)> {
        std::mem::take(&mut *self.artifacts.lock().unwrap())
    }
}

impl ArtifactSink for NestedArtifactSink {
    fn record(&self, artifact: ToolArtifact) -> Result<(), HostEffectError> {
        let bytes = artifact_retained_bytes(&artifact);
        let mut artifacts = self.artifacts.lock().unwrap();
        let mut effects = self.scope.effects.lock().unwrap();
        if effects.finalized {
            return Err(HostEffectError {
                resource: "finalized nested call scope",
                limit_bytes: 0,
            });
        }
        let next = effects.reserved_artifact_bytes.saturating_add(bytes);
        if next > self.scope.limits.artifact_bytes {
            return Err(HostEffectError {
                resource: "nested tool artifacts",
                limit_bytes: self.scope.limits.artifact_bytes,
            });
        }
        effects.reserved_artifact_bytes = next;
        artifacts.push((artifact, bytes));
        Ok(())
    }

    fn take(&self) -> Vec<ToolArtifact> {
        self.take_sized()
            .into_iter()
            .map(|(artifact, _)| artifact)
            .collect()
    }
}

fn artifact_retained_bytes(artifact: &ToolArtifact) -> usize {
    match artifact {
        ToolArtifact::FileDiff { path, patch, .. } => path
            .len()
            .saturating_add(patch.len())
            .saturating_add(std::mem::size_of::<super::llm::FileChangeOperation>()),
    }
}

/// A single nested tool call and its isolated effects.
pub struct NestedToolCall {
    scope: NestedCallScope,
    state: Arc<NestedThreadState>,
    artifacts: Arc<NestedArtifactSink>,
    context: ToolCallContext,
    committed: bool,
}

impl NestedToolCall {
    pub fn context(&self) -> ToolCallContext {
        self.context.clone()
    }

    /// Merge this successful child's effects into the script scope only.
    pub fn commit(mut self) {
        let writes = std::mem::take(&mut *self.state.writes.lock().unwrap());
        let artifacts = self.artifacts.take_sized();
        let mut effects = self.scope.0.effects.lock().unwrap();
        if effects.finalized {
            let state_bytes = writes.values().map(|(_, bytes)| *bytes).sum::<usize>();
            let artifact_bytes = artifacts.iter().map(|(_, bytes)| *bytes).sum::<usize>();
            effects.reserved_state_bytes = effects.reserved_state_bytes.saturating_sub(state_bytes);
            effects.reserved_artifact_bytes = effects
                .reserved_artifact_bytes
                .saturating_sub(artifact_bytes);
            self.committed = true;
            return;
        }
        for (kind, value) in writes {
            if let Some((_, old_bytes)) = effects.state.insert(kind, value) {
                effects.reserved_state_bytes =
                    effects.reserved_state_bytes.saturating_sub(old_bytes);
            }
        }
        effects
            .artifacts
            .extend(artifacts.into_iter().map(|(artifact, _)| artifact));
        self.committed = true;
    }
}

impl Drop for NestedToolCall {
    fn drop(&mut self) {
        if !self.committed {
            self.state.discard();
            self.artifacts.discard();
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
        let started = std::time::Instant::now();
        let span = info_span!(
            "execute_tool",
            tool = self.name(),
            input_bytes = input.len(),
            output_bytes = tracing::field::Empty,
            status = tracing::field::Empty,
            error_category = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        );
        let params: T::Parameters = match serde_json::from_str(&input) {
            Ok(input) => input,
            Err(err) => {
                let reason = format!("{}", err);
                span.record("status", "error");
                span.record("error_category", "invalid_parameters");
                span.record("duration_ms", started.elapsed().as_millis() as u64);
                return Box::pin(
                    async move { ToolError::InvalidParameters(reason).into() }.instrument(span),
                );
            }
        };

        Box::pin(
            async move {
                info!("executing tool");
                let result = self
                    .0
                    .execute(params, ctx)
                    .await
                    .map(|output| output.to_string());
                let span = Span::current();
                match &result {
                    Ok(output) => {
                        span.record("status", "ok");
                        span.record("output_bytes", output.len());
                    }
                    Err(ToolError::InvalidParameters(_)) => {
                        span.record("status", "error");
                        span.record("error_category", "invalid_parameters");
                    }
                    Err(ToolError::ExecutionError(_)) => {
                        span.record("status", "error");
                        span.record("error_category", "execution");
                    }
                    Err(ToolError::ResourceLimit(_)) => {
                        span.record("status", "error");
                        span.record("error_category", "resource_limit");
                    }
                    Err(ToolError::Aborted(_)) => {
                        span.record("status", "error");
                        span.record("error_category", "aborted");
                    }
                };
                span.record("duration_ms", started.elapsed().as_millis() as u64);
                result
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

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;
