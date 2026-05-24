use std::pin::Pin;
use std::sync::Arc;

use rmcp::model::Tool;

use coda_core::tool::{ToolError, ToolObject, ToolResult};

use crate::McpClient;

pub(crate) struct McpToolAdapter {
    client: Arc<McpClient>,
    name: String,
    raw_name: String,
    description: String,
    parameter_schema: serde_json::Value,
}

impl McpToolAdapter {
    pub(crate) fn new(client: Arc<McpClient>, prefix: &str, tool: Tool) -> Self {
        let raw_name = tool.name.into_owned();
        let name = format!("mcp__{prefix}__{raw_name}");
        let description = tool.description.map(|d| d.into_owned()).unwrap_or_default();
        let parameter_schema = serde_json::Value::Object(tool.input_schema.as_ref().clone());

        McpToolAdapter {
            client,
            name,
            raw_name,
            description,
            parameter_schema,
        }
    }
}

impl ToolObject for McpToolAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        &self.parameter_schema
    }

    fn execute(
        self: Arc<Self>,
        params: String,
    ) -> Pin<Box<dyn Future<Output = ToolResult<String>> + Send>> {
        Box::pin(async move {
            let arguments: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&params).map_err(|e| {
                    ToolError::InvalidParameters(format!("invalid JSON arguments: {e}"))
                })?;

            self.client
                .call_tool(&self.raw_name, arguments)
                .await
                .map_err(|e| ToolError::ExecutionError(e.to_string()))
        })
    }
}
