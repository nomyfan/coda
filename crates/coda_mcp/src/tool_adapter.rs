use std::pin::Pin;
use std::sync::Arc;

use rmcp::model::Tool;
use tracing::warn;

use coda_core::tool::{ToolError, ToolObject, ToolResult};

use crate::McpClient;

pub(crate) struct McpToolAdapter {
    client: Arc<McpClient>,
    name: String,
    raw_name: String,
    description: String,
    parameter_schema: serde_json::Value,
}

const NAME_PREFIX: &str = "mcp__";
const MAX_LEN: usize = 64;

fn sanitize_tool_name(prefix: &str, raw_name: &str) -> String {
    let body = format!("{prefix}__{raw_name}");
    let sanitized_body: String = body
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let full = format!("{NAME_PREFIX}{sanitized_body}");
    if full.len() <= MAX_LEN {
        full
    } else {
        let hash = {
            let mut h: u64 = 5381;
            for b in full.as_bytes() {
                h = h.wrapping_mul(31).wrapping_add(*b as u64);
            }
            h
        };
        let suffix = format!("_{hash:08x}", hash = hash as u32);
        let budget = MAX_LEN - NAME_PREFIX.len() - suffix.len();
        let truncated_body = &sanitized_body[..budget];
        let result = format!("{NAME_PREFIX}{truncated_body}{suffix}");
        warn!(
            original = %full,
            truncated = %result,
            "MCP tool name exceeds {MAX_LEN} chars, truncated"
        );
        result
    }
}

impl McpToolAdapter {
    pub(crate) fn new(client: Arc<McpClient>, prefix: &str, tool: Tool) -> Self {
        let raw_name = tool.name.into_owned();
        let name = sanitize_tool_name(prefix, &raw_name);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_invalid_chars() {
        assert_eq!(
            sanitize_tool_name("server", "read.file"),
            "mcp__server__read_file"
        );
    }

    #[test]
    fn sanitize_preserves_valid_chars() {
        assert_eq!(
            sanitize_tool_name("fs", "list-dir"),
            "mcp__fs__list-dir"
        );
    }

    #[test]
    fn sanitize_truncates_long_names() {
        let long_name = "a".repeat(100);
        let result = sanitize_tool_name("server", &long_name);
        assert!(result.len() <= 64);
        assert!(result.starts_with("mcp__"));
        assert!(result.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    }

    #[test]
    fn sanitize_stable_hash_on_truncation() {
        let long_name = "a".repeat(100);
        let r1 = sanitize_tool_name("server", &long_name);
        let r2 = sanitize_tool_name("server", &long_name);
        assert_eq!(r1, r2);
    }
}
