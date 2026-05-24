mod tool_adapter;

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::info;

use coda_core::tool::ToolObject;
use tool_adapter::McpToolAdapter;

type RoleClient = rmcp::RoleClient;

#[derive(Debug)]
pub enum McpError {
    Connect(String),
    Protocol(String),
    Tool(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Connect(msg) => write!(f, "MCP connect error: {msg}"),
            McpError::Protocol(msg) => write!(f, "MCP protocol error: {msg}"),
            McpError::Tool(msg) => write!(f, "MCP tool error: {msg}"),
        }
    }
}

impl std::error::Error for McpError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
}

pub(crate) struct McpClient {
    service: RunningService<RoleClient, ()>,
}

impl McpClient {
    pub(crate) async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, McpError> {
        let result = self
            .service
            .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(arguments))
            .await
            .map_err(|e| McpError::Protocol(e.to_string()))?;

        let text: String = result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.text.as_ref())
            .collect::<Vec<_>>()
            .join("\n");

        if result.is_error == Some(true) {
            Err(McpError::Tool(text))
        } else {
            Ok(text)
        }
    }
}

pub struct McpServer {
    client: Arc<McpClient>,
    prefix: String,
    tools: Vec<Tool>,
}

impl McpServer {
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        let args = config.args.clone();
        let env = config.env.clone();
        let transport =
            TokioChildProcess::new(Command::new(&config.command).configure(move |cmd| {
                cmd.args(&args);
                for (k, v) in &env {
                    cmd.env(k, v);
                }
            }))
            .map_err(|e| McpError::Connect(e.to_string()))?;

        let service =
            ().serve(transport)
                .await
                .map_err(|e| McpError::Connect(e.to_string()))?;

        if let Some(info) = service.peer_info() {
            info!(
                server = config.name,
                name = ?info.server_info.name,
                version = ?info.server_info.version,
                "MCP server connected"
            );
        }

        let tools = service
            .list_all_tools()
            .await
            .map_err(|e| McpError::Protocol(e.to_string()))?;

        info!(
            server = config.name,
            count = tools.len(),
            "discovered MCP tools"
        );

        Ok(McpServer {
            client: Arc::new(McpClient { service }),
            prefix: config.name.clone(),
            tools,
        })
    }

    /// Create fresh `ToolObject` instances for all tools on this server.
    pub fn tool_objects(&self) -> Vec<Box<dyn ToolObject>> {
        self.tools
            .iter()
            .map(|tool| {
                Box::new(McpToolAdapter::new(
                    self.client.clone(),
                    &self.prefix,
                    tool.clone(),
                )) as Box<dyn ToolObject>
            })
            .collect()
    }

    pub async fn shutdown(self) -> Result<(), McpError> {
        let mut client = self.client;
        for _ in 0..10 {
            match Arc::try_unwrap(client) {
                Ok(c) => {
                    return c
                        .service
                        .cancel()
                        .await
                        .map_err(|e| McpError::Protocol(e.to_string()))
                        .map(|_| ());
                }
                Err(arc) => {
                    client = arc;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        Err(McpError::Protocol(
            "cannot shutdown: tool adapters still hold references after retries".into(),
        ))
    }
}
