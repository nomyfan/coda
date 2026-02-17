use std::boxed::Box;
use std::collections::BTreeMap;
use std::fmt::Display;
use std::pin::Pin;
use std::sync::Arc;

use serde::de::DeserializeOwned;

use super::llm::ToolDefinition;

#[derive(Debug)]
pub(crate) enum ToolError {
    InvalidParameters(String),
    ExecutionError(String),
}

impl Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::InvalidParameters(reason) => write!(f, "Invalid parameters: {}", reason),
            ToolError::ExecutionError(reason) => write!(f, "Execution error: {}", reason),
        }
    }
}

pub(crate) type ToolResult<T> = Result<T, ToolError>;

impl<T> From<ToolError> for ToolResult<T> {
    fn from(value: ToolError) -> Self {
        Err(value)
    }
}

pub(crate) trait Tool: Send + Sync + 'static {
    type Parameters: DeserializeOwned + Send;
    type Output: Display + Send;

    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameter_schema(&self) -> &serde_json::Value;
    fn execute(
        &self,
        params: Self::Parameters,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static;
}

pub(crate) trait ToolObject {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameter_schema(&self) -> &serde_json::Value;
    fn execute(
        self: Arc<Self>,
        params: String,
    ) -> Pin<Box<dyn Future<Output = ToolResult<String>> + Send>>;
}

struct ToolWrapper<T: Tool>(T);

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
    ) -> Pin<Box<dyn Future<Output = ToolResult<String>> + Send>> {
        let params: T::Parameters = match serde_json::from_str(&input) {
            Ok(input) => input,
            Err(err) => {
                let reason = format!("{}", err);
                return Box::pin(async move { ToolError::InvalidParameters(reason).into() });
            }
        };

        Box::pin(async move {
            let output = self.0.execute(params).await?;
            Ok(output.to_string())
        })
    }
}

impl<T: Tool> From<T> for ToolWrapper<T> {
    fn from(value: T) -> Self {
        ToolWrapper(value)
    }
}

pub(crate) struct ToolManager {
    tools: BTreeMap<String, Arc<dyn ToolObject>>,
}

impl ToolManager {
    pub(crate) fn new() -> Self {
        ToolManager {
            tools: BTreeMap::new(),
        }
    }

    pub(crate) fn register<T: Tool>(&mut self, tool: T) {
        self.tools
            .insert(tool.name().to_string(), Arc::new(ToolWrapper::from(tool)));
    }

    pub(crate) fn get(&self, name: &str) -> Option<Arc<dyn ToolObject>> {
        self.tools.get(name).cloned()
    }

    pub(crate) fn descriptors(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameter_schema: tool.parameter_schema().clone(),
            })
            .collect()
    }
}
