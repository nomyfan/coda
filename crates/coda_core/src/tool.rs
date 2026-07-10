use std::boxed::Box;
use std::collections::BTreeMap;
use std::fmt::{Debug, Display};
use std::pin::Pin;
use std::sync::Arc;

use serde::de::DeserializeOwned;
pub use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, info, info_span};

use super::llm::ToolDefinition;

/// Per-invocation execution context handed to every tool call.
///
/// `cancel` fires when the caller aborts the invocation (e.g. the user aborts
/// the turn). Tools driving external work — child processes, network calls —
/// should observe it, tear that work down, and return [`ToolError::Aborted`]
/// promptly; quick in-process tools may ignore it and run to completion.
#[derive(Clone, Debug, Default)]
pub struct ToolCallContext {
    pub cancel: CancellationToken,
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
